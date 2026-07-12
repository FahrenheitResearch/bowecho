//! Streaming native-level HRRR/RRFS input for forecast radar volumes.
//!
//! SimSat's compact `.ssb` brick deliberately retains only radiative-transfer
//! channels and surface winds.  A radar forward operator needs the native
//! three-dimensional wind, height, thermodynamic, turbulence and hydrometeor
//! fields instead.  This reader indexes a GRIB2 stream once, crops it around a
//! requested virtual radar, and decodes one field at a time.  Hydrometeor
//! species are requested independently so the caller can fold each species
//! into additive scattering state and release it before reading the next one.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use grib_core::grib2::{Grib2File, GridDefinition, grid_latlon, unpack_message};
use simsat::ingest_grib::{
    CODE_CIMIXR, CODE_CLMR, CODE_GRLE, CODE_HGT, CODE_ICMR, CODE_NCCICE, CODE_NCONCD, CODE_PRES,
    CODE_RWMR, CODE_SNMR, CODE_SPFH, CODE_SPNCR, CODE_TMP, CODE_UGRD, CODE_VGRD, FieldCode,
    LEVEL_HYBRID, LEVEL_SURFACE, MessageLocation, REQUIRED_SCAN_MODE, index_grib_messages,
};

/// Pressure vertical velocity (omega, Pa/s), not geometric `w`.
const CODE_VVEL: FieldCode = FieldCode {
    discipline: 0,
    category: 2,
    number: 8,
};
const CODE_TKE: FieldCode = FieldCode {
    discipline: 0,
    category: 19,
    number: 11,
};
const RESOLUTION_COMPONENTS_GRID_RELATIVE: u8 = 0x08;
const GRAVITY_MPS2: f32 = 9.806_65;

#[derive(Debug)]
pub enum OperationalRadarGribError {
    Io(std::io::Error),
    Grib(String),
    MissingField(String),
    Shape(String),
    Unsupported(String),
}

impl std::fmt::Display for OperationalRadarGribError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "GRIB I/O error: {error}"),
            Self::Grib(message) => write!(formatter, "GRIB decode error: {message}"),
            Self::MissingField(message) => write!(formatter, "required field missing: {message}"),
            Self::Shape(message) => write!(formatter, "unsupported GRIB geometry: {message}"),
            Self::Unsupported(message) => write!(formatter, "unsupported model input: {message}"),
        }
    }
}

impl std::error::Error for OperationalRadarGribError {}

impl From<std::io::Error> for OperationalRadarGribError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationalModel {
    Hrrr,
    Rrfs,
}

impl OperationalModel {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Hrrr => "HRRR",
            Self::Rrfs => "RRFS",
        }
    }
}

/// The microphysics interpretation applied to native operational-model GRIB.
///
/// GRIB2 carries categories, not WRF's `MP_PHYSICS` global attribute.  These
/// identifiers therefore describe an explicit BowEcho mapping rather than
/// claiming that the original scheme configuration was recovered from the
/// file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationalSchemeAssumption {
    /// HRRR's native Thompson-family mass categories, using native number
    /// fields where they are complete and the versioned bulk closure for the
    /// remaining categories.
    HrrrThompsonCategoryBulkV1,
    /// RRFS categories mapped by field identity.  No specific RRFS
    /// microphysics scheme is inferred when the GRIB inventory does not carry
    /// one; missing number fields use the versioned bulk closure.
    RrfsCategoryBulkV1,
}

impl OperationalSchemeAssumption {
    pub const fn stable_id(self) -> &'static str {
        match self {
            Self::HrrrThompsonCategoryBulkV1 => "hrrr-thompson-category-bulk-rayleigh-sband-v1",
            Self::RrfsCategoryBulkV1 => "rrfs-category-bulk-rayleigh-sband-v1",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::HrrrThompsonCategoryBulkV1 => {
                "HRRR Thompson-family categories; native complete number fields plus documented bulk PSD defaults"
            }
            Self::RrfsCategoryBulkV1 => {
                "RRFS GRIB categories; no unencoded scheme identity is inferred and absent number fields use documented bulk PSD defaults"
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationalFieldOrigin {
    NativeGrib,
    DerivedFromNativeOmega,
    AssumedZero,
    BulkPsdClosure,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationalSpeciesProvenance {
    pub kind: OperationalHydrometeor,
    pub mass_field: &'static str,
    pub number_field: Option<&'static str>,
    pub number_origin: OperationalFieldOrigin,
    pub populated_cells: usize,
}

/// Audit trail carried with every operational-model scattering state.  It is
/// deliberately detailed enough for a future Level-II export to explain the
/// input inventory and every closure assumption without reopening the GRIB.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationalRadarProvenance {
    pub model: OperationalModel,
    pub source_path: PathBuf,
    pub valid_time: DateTime<Utc>,
    pub scheme_assumption: OperationalSchemeAssumption,
    pub scattering_kernel: &'static str,
    pub species: Vec<OperationalSpeciesProvenance>,
    pub vertical_velocity_origin: OperationalFieldOrigin,
    pub horizontal_winds_rotated_from_grid: bool,
    pub tke_origin: Option<OperationalFieldOrigin>,
    pub notes: Vec<String>,
}

/// Builder-neutral native-model state for the future Level-II renderer.
///
/// Geometry and winds use the same flattened `[level, row, column]` contract as
/// `WrfRadarFields`.  `linear_scattering` is intentionally retained in
/// additive linear space: a WRF-radar adapter can compact it into the existing
/// polar state without ever interpolating dBZ, ZDR, or rhoHV directly.
pub struct OperationalRadarState {
    pub nx: usize,
    pub ny: usize,
    pub nz: usize,
    pub lat: Vec<f32>,
    pub lon: Vec<f32>,
    pub height_msl: Vec<f32>,
    pub dbz: Vec<f32>,
    pub u: Vec<f32>,
    pub v: Vec<f32>,
    pub w: Vec<f32>,
    pub terrain_m: Vec<f32>,
    pub dx_m: Option<f64>,
    pub tke_m2s2: Option<Vec<f32>>,
    pub linear_scattering: Vec<crate::wrf_radar_physics::BulkContribution>,
    pub scheme_profile: crate::wrf_radar_physics::SchemeProfile,
    pub provenance: OperationalRadarProvenance,
}

impl OperationalRadarState {
    pub fn cell_count(&self) -> usize {
        self.nx * self.ny * self.nz
    }
}

/// Stable, UI-ready view of a compatible native HRRR file already found by
/// the shared SimSat cache scanner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CachedOperationalHrrrInput {
    pub path: PathBuf,
    pub label: String,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum OperationalHydrometeor {
    CloudLiquid,
    CloudIce,
    Rain,
    Snow,
    Graupel,
}

impl OperationalHydrometeor {
    pub const ALL: [Self; 5] = [
        Self::CloudLiquid,
        Self::CloudIce,
        Self::Rain,
        Self::Snow,
        Self::Graupel,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::CloudLiquid => "cloud liquid",
            Self::CloudIce => "cloud ice",
            Self::Rain => "rain",
            Self::Snow => "snow",
            Self::Graupel => "graupel",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CatalogEntry {
    location_index: usize,
    submessage_index: usize,
    code: FieldCode,
    level_type: u8,
    level: u32,
}

#[derive(Debug)]
struct OperationalCatalog {
    locations: Vec<MessageLocation>,
    entries: Vec<CatalogEntry>,
    grid: GridDefinition,
    reference_time: NaiveDateTime,
    forecast_time: u32,
    time_range_unit: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RadarCrop {
    pub i0: usize,
    pub i1: usize,
    pub j0: usize,
    pub j1: usize,
}

impl RadarCrop {
    pub fn nx(self) -> usize {
        self.i1 - self.i0 + 1
    }

    pub fn ny(self) -> usize {
        self.j1 - self.j0 + 1
    }

    fn around_radar(
        lat: &[f32],
        lon: &[f32],
        nx: usize,
        ny: usize,
        site_lat_deg: f64,
        site_lon_deg: f64,
        radius_km: f64,
    ) -> Result<Self, OperationalRadarGribError> {
        if lat.len() != nx * ny || lon.len() != nx * ny {
            return Err(OperationalRadarGribError::Shape(
                "latitude/longitude plane size does not match the GRIB grid".to_owned(),
            ));
        }
        if !site_lat_deg.is_finite()
            || !site_lon_deg.is_finite()
            || !radius_km.is_finite()
            || radius_km <= 0.0
        {
            return Err(OperationalRadarGribError::Shape(
                "virtual-radar coordinate and crop radius must be finite and positive".to_owned(),
            ));
        }

        // Keep one broad pulse/beam support margin around the requested radar
        // range.  The rectangular hull may contain cells outside the circle;
        // the forward operator's own domain-coverage calculation handles them.
        let admission_km = radius_km + 75.0;
        let mut i0 = nx;
        let mut i1 = 0usize;
        let mut j0 = ny;
        let mut j1 = 0usize;
        let mut found = false;
        for j in 0..ny {
            for i in 0..nx {
                let index = j * nx + i;
                let distance = great_circle_distance_km(
                    site_lat_deg,
                    site_lon_deg,
                    f64::from(lat[index]),
                    f64::from(lon[index]),
                );
                if distance <= admission_km {
                    found = true;
                    i0 = i0.min(i);
                    i1 = i1.max(i);
                    j0 = j0.min(j);
                    j1 = j1.max(j);
                }
            }
        }
        if !found {
            return Err(OperationalRadarGribError::Shape(format!(
                "virtual radar ({site_lat_deg:.3}, {site_lon_deg:.3}) is outside the model domain"
            )));
        }
        Ok(Self { i0, i1, j0, j1 })
    }
}

#[derive(Clone, Debug)]
pub struct OperationalRadarProbe {
    pub model: OperationalModel,
    pub scheme_assumption: OperationalSchemeAssumption,
    pub nx: usize,
    pub ny: usize,
    pub nz: usize,
    pub valid_time: DateTime<Utc>,
    pub available_species: BTreeSet<OperationalHydrometeor>,
    pub has_vertical_velocity: bool,
    pub has_tke: bool,
}

/// Base fields retained while species are streamed through the scattering
/// closure.  Arrays use `[level, row, column]` on the cropped native hybrid grid.
pub struct OperationalRadarBaseFields {
    pub model: OperationalModel,
    pub valid_time: DateTime<Utc>,
    pub nx: usize,
    pub ny: usize,
    pub nz: usize,
    pub lat: Vec<f32>,
    pub lon: Vec<f32>,
    pub height_msl_m: Vec<f32>,
    pub temperature_k: Vec<f32>,
    pub air_density_kgm3: Vec<f32>,
    pub u_mps: Vec<f32>,
    pub v_mps: Vec<f32>,
    pub w_mps: Vec<f32>,
    pub tke_m2s2: Option<Vec<f32>>,
    pub terrain_m: Vec<f32>,
}

pub struct OperationalSpeciesField {
    pub kind: OperationalHydrometeor,
    pub mixing_ratio_kgkg: Vec<f32>,
    pub number_per_kg: Option<Vec<f32>>,
}

/// One open, cropped native-model stream.  Call `read_base_fields` once, then
/// `read_species` in any order; each returned species can be released before
/// reading the next one.
pub struct OperationalRadarGribReader {
    path: PathBuf,
    file: File,
    catalog: OperationalCatalog,
    crop: RadarCrop,
    lat: Vec<f32>,
    lon: Vec<f32>,
    model: OperationalModel,
    nz: usize,
    valid_time: DateTime<Utc>,
    buffer: Vec<u8>,
}

/// Discover HRRR native files through the same cache contract SimSat uses, so
/// the future-Level-II path never creates a competing download directory.
pub(crate) fn discover_cached_hrrr_inputs(root: &Path) -> Vec<CachedOperationalHrrrInput> {
    crate::simsat_hrrr::discover_native_files(root)
        .into_iter()
        .map(|input| CachedOperationalHrrrInput {
            label: input.label(),
            path: input.path,
            bytes: input.bytes,
        })
        .collect()
}

/// Reuse SimSat's publication-aware latest-cycle choices for the requested
/// forecast hour.
pub(crate) fn latest_hrrr_native_specs(
    now: DateTime<Utc>,
    forecast_hour: u16,
) -> Vec<crate::simsat_hrrr::HrrrNativeSpec> {
    crate::simsat_hrrr::latest_specs(now, forecast_hour)
}

/// Reuse SimSat's resumable NOMADS/AWS downloader.  `Ok(None)` is cancellation,
/// while every corrupt/incomplete terminal state remains an error.
pub(crate) fn acquire_hrrr_native_input(
    spec: &crate::simsat_hrrr::HrrrNativeSpec,
    root: &Path,
    cancel: &AtomicBool,
    progress: &dyn Fn(&str),
) -> Result<Option<PathBuf>, OperationalRadarGribError> {
    let outcome = crate::simsat_hrrr::download_native_with_status(spec, root, cancel, |status| {
        progress(&status.message)
    })
    .map_err(|error| OperationalRadarGribError::Io(std::io::Error::other(error.to_string())))?;
    if outcome.is_ready() {
        Ok(Some(outcome.path))
    } else if outcome.is_cancelled() {
        Ok(None)
    } else {
        Err(OperationalRadarGribError::Unsupported(
            "HRRR native download ended without a usable file".to_owned(),
        ))
    }
}

/// One-shot entry point for a local HRRR/RRFS native GRIB.  The returned state
/// is ready for the narrow `WrfRadarFields` adapter; it does not build an
/// independent radar-volume implementation.
pub fn read_operational_radar_state(
    path: &Path,
    site_lat_deg: f64,
    site_lon_deg: f64,
    maximum_range_km: f64,
    progress: &dyn Fn(&str),
) -> Result<OperationalRadarState, OperationalRadarGribError> {
    let mut reader = OperationalRadarGribReader::open_for_radar(
        path,
        site_lat_deg,
        site_lon_deg,
        maximum_range_km,
    )?;
    reader.read_radar_state(progress)
}

impl OperationalRadarGribReader {
    pub fn probe(path: &Path) -> Result<OperationalRadarProbe, OperationalRadarGribError> {
        let catalog = build_catalog(path)?;
        let nz = complete_level_count(&catalog, CODE_TMP, "TMP")?;
        let model = detect_operational_model(path, &catalog, nz)?;
        let available_species = OperationalHydrometeor::ALL
            .into_iter()
            .filter(|kind| {
                species_mass_code(*kind, model).is_some_and(|code| has_complete(&catalog, code, nz))
            })
            .collect();
        Ok(OperationalRadarProbe {
            model,
            scheme_assumption: scheme_assumption(model),
            nx: catalog.grid.nx as usize,
            ny: catalog.grid.ny as usize,
            nz,
            valid_time: catalog_valid_time(&catalog)?,
            available_species,
            has_vertical_velocity: has_complete(&catalog, CODE_VVEL, nz),
            has_tke: has_complete(&catalog, CODE_TKE, nz),
        })
    }

    pub fn open_for_radar(
        path: &Path,
        site_lat_deg: f64,
        site_lon_deg: f64,
        maximum_range_km: f64,
    ) -> Result<Self, OperationalRadarGribError> {
        let catalog = build_catalog(path)?;
        let nx = catalog.grid.nx as usize;
        let ny = catalog.grid.ny as usize;
        let (lat_full, lon_full) = latlon_planes(&catalog.grid)?;
        let crop = RadarCrop::around_radar(
            &lat_full,
            &lon_full,
            nx,
            ny,
            site_lat_deg,
            site_lon_deg,
            maximum_range_km,
        )?;
        let lat = crop_plane(&lat_full, nx, crop);
        let lon = crop_plane(&lon_full, nx, crop);
        let nz = complete_level_count(&catalog, CODE_TMP, "TMP")?;
        for (code, name) in [
            (CODE_HGT, "HGT"),
            (CODE_PRES, "PRES"),
            (CODE_SPFH, "SPFH"),
            (CODE_UGRD, "UGRD"),
            (CODE_VGRD, "VGRD"),
        ] {
            let count = complete_level_count(&catalog, code, name)?;
            if count != nz {
                return Err(OperationalRadarGribError::Shape(format!(
                    "{name} carries {count} hybrid levels, but TMP carries {nz}"
                )));
            }
        }
        let model = detect_operational_model(path, &catalog, nz)?;
        validate_scattering_inventory(&catalog, model, nz)?;
        let valid_time = catalog_valid_time(&catalog)?;
        Ok(Self {
            path: path.to_path_buf(),
            file: File::open(path)?,
            catalog,
            crop,
            lat,
            lon,
            model,
            nz,
            valid_time,
            buffer: Vec::new(),
        })
    }

    pub fn source_path(&self) -> &Path {
        &self.path
    }

    pub fn model(&self) -> OperationalModel {
        self.model
    }

    pub fn valid_time(&self) -> DateTime<Utc> {
        self.valid_time.to_owned()
    }

    pub fn read_base_fields(
        &mut self,
        progress: &dyn Fn(&str),
    ) -> Result<OperationalRadarBaseFields, OperationalRadarGribError> {
        progress("reading native model height");
        let height_msl_m = self.read_required_hybrid(CODE_HGT, "HGT")?;
        progress("reading native model temperature and density");
        let temperature_k = self.read_required_hybrid(CODE_TMP, "TMP")?;
        let mut air_density_kgm3 = self.read_required_hybrid(CODE_PRES, "PRES")?;
        let specific_humidity = self.read_required_hybrid(CODE_SPFH, "SPFH")?;
        for ((density, &temperature), &specific_humidity) in air_density_kgm3
            .iter_mut()
            .zip(&temperature_k)
            .zip(&specific_humidity)
        {
            let pressure = *density;
            let mixing_ratio = specific_humidity_to_mixing_ratio(specific_humidity);
            *density = crate::wrf_radar_physics::air_density(pressure, temperature, mixing_ratio);
        }
        progress("reading native model wind");
        let mut u_mps = self.read_required_hybrid(CODE_UGRD, "UGRD")?;
        let mut v_mps = self.read_required_hybrid(CODE_VGRD, "VGRD")?;
        if self.catalog.grid.resolution_flags & RESOLUTION_COMPONENTS_GRID_RELATIVE != 0 {
            rotate_grid_relative_winds(
                &mut u_mps,
                &mut v_mps,
                &self.lat,
                &self.lon,
                self.crop.nx(),
                self.crop.ny(),
                self.nz,
            )?;
        }
        let w_mps = match self.read_optional_hybrid(CODE_VVEL, "VVEL")? {
            Some(omega_pa_s) => {
                omega_to_geometric_vertical_velocity(&omega_pa_s, &air_density_kgm3)?
            }
            None => vec![0.0; height_msl_m.len()],
        };
        let tke_m2s2 = self.read_optional_hybrid(CODE_TKE, "TKE")?;
        let terrain_m = self.read_surface(CODE_HGT, "HGT at surface")?;
        Ok(OperationalRadarBaseFields {
            model: self.model,
            valid_time: self.valid_time.to_owned(),
            nx: self.crop.nx(),
            ny: self.crop.ny(),
            nz: self.nz,
            lat: self.lat.clone(),
            lon: self.lon.clone(),
            height_msl_m,
            temperature_k,
            air_density_kgm3,
            u_mps,
            v_mps,
            w_mps,
            tke_m2s2,
            terrain_m,
        })
    }

    pub fn read_species(
        &mut self,
        kind: OperationalHydrometeor,
    ) -> Result<Option<OperationalSpeciesField>, OperationalRadarGribError> {
        let Some(mass_code) = species_mass_code(kind, self.model) else {
            return Ok(None);
        };
        let Some(mixing_ratio_kgkg) = self.read_optional_hybrid(mass_code, kind.label())? else {
            return Ok(None);
        };
        let number_per_kg = match species_number_code(kind) {
            Some(code) => self.read_optional_hybrid(code, "number concentration")?,
            None => None,
        };
        Ok(Some(OperationalSpeciesField {
            kind,
            mixing_ratio_kgkg,
            number_per_kg,
        }))
    }

    /// Read the native dynamics once, stream each hydrometeor through the
    /// versioned bulk scattering closure, and return a builder-neutral state.
    /// Unsupported or internally incomplete inventories fail before any
    /// scientific product is emitted.
    pub fn read_radar_state(
        &mut self,
        progress: &dyn Fn(&str),
    ) -> Result<OperationalRadarState, OperationalRadarGribError> {
        let expected = self.crop.nx() * self.crop.ny() * self.nz;
        let base = self.read_base_fields(progress)?;
        validate_base_shapes(&base, expected)?;

        let inventory = scattering_inventory(&self.catalog, self.model, self.nz)?;
        let assumption = scheme_assumption(self.model);
        let present_fields = inventory
            .iter()
            .flat_map(|item| {
                std::iter::once(wrf_mass_field_name(item.kind)).chain(item.number_field)
            })
            .collect::<Vec<_>>();
        let scheme_profile = match assumption {
            OperationalSchemeAssumption::HrrrThompsonCategoryBulkV1 => {
                // HRRR GRIB does not preserve MP_PHYSICS.  ID 8 selects the
                // documented Thompson-family bulk coefficients, while the
                // provenance above makes clear this is an operational mapping.
                crate::wrf_radar_physics::detect_scheme(Some(8), &present_fields)
            }
            OperationalSchemeAssumption::RrfsCategoryBulkV1 => {
                crate::wrf_radar_physics::detect_scheme(None, &present_fields)
            }
        };
        if scheme_profile.capability
            == crate::wrf_radar_physics::MicrophysicsCapability::Unsupported
        {
            return Err(OperationalRadarGribError::Unsupported(format!(
                "{} cannot represent the complete native hydrometeor inventory",
                assumption.stable_id()
            )));
        }

        let mut linear_scattering =
            vec![crate::wrf_radar_physics::BulkContribution::default(); expected];
        let mut species_provenance = Vec::with_capacity(inventory.len());
        for inventory_item in inventory {
            progress(&format!(
                "scattering native {} ({})",
                inventory_item.kind.label(),
                mass_field_name(inventory_item.kind, self.model)
            ));
            let species = self.read_species(inventory_item.kind)?.ok_or_else(|| {
                OperationalRadarGribError::MissingField(format!(
                    "{} disappeared after inventory validation",
                    inventory_item.kind.label()
                ))
            })?;
            validate_species_shape(&species, expected)?;
            let mut populated_cells = 0usize;
            for index in 0..expected {
                let q = species.mixing_ratio_kgkg[index];
                if !q.is_finite() || q <= 0.0 {
                    continue;
                }
                let contribution = crate::wrf_radar_physics::bulk_sband_contribution(
                    crate::wrf_radar_physics::BulkSpeciesInput {
                        kind: wrf_hydrometeor_kind(species.kind),
                        q_kgkg: q,
                        number_per_kg: species.number_per_kg.as_ref().map(|values| values[index]),
                        volume_m3_per_kg: None,
                        temperature_k: base.temperature_k[index],
                        air_density_kgm3: base.air_density_kgm3[index],
                    },
                    &scheme_profile,
                );
                if contribution.zh <= 0.0 {
                    continue;
                }
                populated_cells += 1;
                linear_scattering[index] =
                    merge_linear_scattering(linear_scattering[index], contribution);
            }
            species_provenance.push(OperationalSpeciesProvenance {
                kind: species.kind,
                mass_field: mass_field_name(species.kind, self.model),
                number_field: inventory_item.number_field,
                number_origin: if inventory_item.number_field.is_some() {
                    OperationalFieldOrigin::NativeGrib
                } else {
                    OperationalFieldOrigin::BulkPsdClosure
                },
                populated_cells,
            });
            // `species` drops here before the next native field is decoded.
        }

        if linear_scattering
            .iter()
            .all(|contribution| contribution.zh <= 0.0)
        {
            return Err(OperationalRadarGribError::Unsupported(
                "native hydrometeor fields contain no positive scattering mass in the radar crop"
                    .to_owned(),
            ));
        }
        let dbz = linear_scattering
            .iter()
            .map(|contribution| {
                if contribution.zh.is_finite() && contribution.zh > 0.0 {
                    crate::wrf_radar_physics::z_to_dbz(contribution.zh)
                } else {
                    f32::NAN
                }
            })
            .collect();
        let vertical_velocity_origin = if has_complete(&self.catalog, CODE_VVEL, self.nz) {
            OperationalFieldOrigin::DerivedFromNativeOmega
        } else {
            OperationalFieldOrigin::AssumedZero
        };
        let tke_origin = base
            .tke_m2s2
            .as_ref()
            .map(|_| OperationalFieldOrigin::NativeGrib);
        let mut notes = vec![assumption.description().to_owned()];
        if vertical_velocity_origin == OperationalFieldOrigin::AssumedZero {
            notes.push("vertical velocity was absent; w is explicitly zero".to_owned());
        } else {
            notes.push(
                "geometric w was derived hydrostatically from native VVEL omega and air density"
                    .to_owned(),
            );
        }
        if self.catalog.grid.resolution_flags & RESOLUTION_COMPONENTS_GRID_RELATIVE != 0 {
            notes.push("UGRD/VGRD were rotated from grid axes to earth east/north".to_owned());
        }
        if tke_origin.is_none() {
            notes.push(
                "TKE was absent; spectrum width must not claim a native turbulence term".to_owned(),
            );
        }
        let provenance = OperationalRadarProvenance {
            model: self.model,
            source_path: self.path.clone(),
            valid_time: self.valid_time.to_owned(),
            scheme_assumption: assumption,
            scattering_kernel: crate::wrf_radar_physics::bulk_sband_model_id(),
            species: species_provenance,
            vertical_velocity_origin,
            horizontal_winds_rotated_from_grid: self.catalog.grid.resolution_flags
                & RESOLUTION_COMPONENTS_GRID_RELATIVE
                != 0,
            tke_origin,
            notes,
        };

        Ok(OperationalRadarState {
            nx: base.nx,
            ny: base.ny,
            nz: base.nz,
            lat: base.lat,
            lon: base.lon,
            height_msl: base.height_msl_m,
            dbz,
            u: base.u_mps,
            v: base.v_mps,
            w: base.w_mps,
            terrain_m: base.terrain_m,
            dx_m: operational_grid_spacing_m(self.catalog.grid.template, self.catalog.grid.dx),
            tke_m2s2: base.tke_m2s2,
            linear_scattering,
            scheme_profile,
            provenance,
        })
    }

    fn read_required_hybrid(
        &mut self,
        code: FieldCode,
        name: &str,
    ) -> Result<Vec<f32>, OperationalRadarGribError> {
        self.read_optional_hybrid(code, name)?
            .ok_or_else(|| OperationalRadarGribError::MissingField(name.to_owned()))
    }

    fn read_optional_hybrid(
        &mut self,
        code: FieldCode,
        name: &str,
    ) -> Result<Option<Vec<f32>>, OperationalRadarGribError> {
        let entries = hybrid_entries(&self.catalog, code);
        if entries.is_empty() {
            return Ok(None);
        }
        validate_levels(entries.keys().copied(), name, self.nz)?;
        let plane = self.crop.nx() * self.crop.ny();
        let mut output = vec![f32::NAN; plane * self.nz];
        for (level, entry) in entries {
            let decoded = decode_entry(&mut self.file, &self.catalog, entry, &mut self.buffer)?;
            let cropped = crop_plane_f64(&decoded, self.catalog.grid.nx as usize, self.crop);
            let offset = (level as usize - 1) * plane;
            output[offset..offset + plane].copy_from_slice(&cropped);
        }
        Ok(Some(output))
    }

    fn read_surface(
        &mut self,
        code: FieldCode,
        name: &str,
    ) -> Result<Vec<f32>, OperationalRadarGribError> {
        let entry = self
            .catalog
            .entries
            .iter()
            .find(|entry| entry.code == code && entry.level_type == LEVEL_SURFACE)
            .copied()
            .ok_or_else(|| OperationalRadarGribError::MissingField(name.to_owned()))?;
        let decoded = decode_entry(&mut self.file, &self.catalog, entry, &mut self.buffer)?;
        Ok(crop_plane_f64(
            &decoded,
            self.catalog.grid.nx as usize,
            self.crop,
        ))
    }
}

fn species_mass_code(kind: OperationalHydrometeor, model: OperationalModel) -> Option<FieldCode> {
    Some(match kind {
        OperationalHydrometeor::CloudLiquid => CODE_CLMR,
        OperationalHydrometeor::CloudIce => match model {
            OperationalModel::Hrrr => CODE_CIMIXR,
            OperationalModel::Rrfs => CODE_ICMR,
        },
        OperationalHydrometeor::Rain => CODE_RWMR,
        OperationalHydrometeor::Snow => CODE_SNMR,
        OperationalHydrometeor::Graupel => CODE_GRLE,
    })
}

fn species_number_code(kind: OperationalHydrometeor) -> Option<FieldCode> {
    match kind {
        OperationalHydrometeor::CloudLiquid => Some(CODE_NCONCD),
        OperationalHydrometeor::CloudIce => Some(CODE_NCCICE),
        OperationalHydrometeor::Rain => Some(CODE_SPNCR),
        OperationalHydrometeor::Snow | OperationalHydrometeor::Graupel => None,
    }
}

fn scheme_assumption(model: OperationalModel) -> OperationalSchemeAssumption {
    match model {
        OperationalModel::Hrrr => OperationalSchemeAssumption::HrrrThompsonCategoryBulkV1,
        OperationalModel::Rrfs => OperationalSchemeAssumption::RrfsCategoryBulkV1,
    }
}

fn mass_field_name(kind: OperationalHydrometeor, model: OperationalModel) -> &'static str {
    match kind {
        OperationalHydrometeor::CloudLiquid => "CLMR",
        OperationalHydrometeor::CloudIce => match model {
            OperationalModel::Hrrr => "CIMIXR",
            OperationalModel::Rrfs => "ICMR",
        },
        OperationalHydrometeor::Rain => "RWMR",
        OperationalHydrometeor::Snow => "SNMR",
        OperationalHydrometeor::Graupel => "GRLE",
    }
}

fn number_field_name(kind: OperationalHydrometeor) -> Option<&'static str> {
    match kind {
        OperationalHydrometeor::CloudLiquid => Some("NCONCD"),
        OperationalHydrometeor::CloudIce => Some("NCCICE"),
        OperationalHydrometeor::Rain => Some("SPNCR"),
        OperationalHydrometeor::Snow | OperationalHydrometeor::Graupel => None,
    }
}

fn wrf_mass_field_name(kind: OperationalHydrometeor) -> &'static str {
    match kind {
        OperationalHydrometeor::CloudLiquid => "QCLOUD",
        OperationalHydrometeor::CloudIce => "QICE",
        OperationalHydrometeor::Rain => "QRAIN",
        OperationalHydrometeor::Snow => "QSNOW",
        OperationalHydrometeor::Graupel => "QGRAUP",
    }
}

fn wrf_hydrometeor_kind(kind: OperationalHydrometeor) -> crate::wrf_radar_physics::HydrometeorKind {
    match kind {
        OperationalHydrometeor::CloudLiquid => {
            crate::wrf_radar_physics::HydrometeorKind::CloudWater
        }
        OperationalHydrometeor::CloudIce => crate::wrf_radar_physics::HydrometeorKind::CloudIce,
        OperationalHydrometeor::Rain => crate::wrf_radar_physics::HydrometeorKind::Rain,
        OperationalHydrometeor::Snow => crate::wrf_radar_physics::HydrometeorKind::Snow,
        OperationalHydrometeor::Graupel => crate::wrf_radar_physics::HydrometeorKind::Graupel,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScatteringInventoryItem {
    kind: OperationalHydrometeor,
    number_field: Option<&'static str>,
}

fn optional_complete_field(
    catalog: &OperationalCatalog,
    code: FieldCode,
    name: &str,
    expected: usize,
) -> Result<bool, OperationalRadarGribError> {
    let entries = hybrid_entries(catalog, code);
    if entries.is_empty() {
        return Ok(false);
    }
    validate_levels(entries.keys().copied(), name, expected)?;
    Ok(true)
}

fn scattering_inventory(
    catalog: &OperationalCatalog,
    model: OperationalModel,
    nz: usize,
) -> Result<Vec<ScatteringInventoryItem>, OperationalRadarGribError> {
    let mut inventory = Vec::with_capacity(OperationalHydrometeor::ALL.len());
    for kind in OperationalHydrometeor::ALL {
        let mass_code = species_mass_code(kind, model).expect("all operational species have mass");
        let mass_name = mass_field_name(kind, model);
        if !optional_complete_field(catalog, mass_code, mass_name, nz)? {
            return Err(OperationalRadarGribError::MissingField(format!(
                "{mass_name} ({}) is required by {}",
                kind.label(),
                scheme_assumption(model).stable_id()
            )));
        }
        let number_field = match species_number_code(kind) {
            Some(code)
                if optional_complete_field(
                    catalog,
                    code,
                    number_field_name(kind).expect("number code has a name"),
                    nz,
                )? =>
            {
                number_field_name(kind)
            }
            _ => None,
        };
        inventory.push(ScatteringInventoryItem { kind, number_field });
    }
    Ok(inventory)
}

fn validate_scattering_inventory(
    catalog: &OperationalCatalog,
    model: OperationalModel,
    nz: usize,
) -> Result<(), OperationalRadarGribError> {
    scattering_inventory(catalog, model, nz).map(|_| ())
}

fn detect_operational_model(
    path: &Path,
    catalog: &OperationalCatalog,
    nz: usize,
) -> Result<OperationalModel, OperationalRadarGribError> {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let has_hrrr_ice = optional_complete_field(catalog, CODE_CIMIXR, "CIMIXR", nz)?;
    let has_rrfs_ice = optional_complete_field(catalog, CODE_ICMR, "ICMR", nz)?;
    classify_operational_model(&filename, has_hrrr_ice, has_rrfs_ice)
}

fn classify_operational_model(
    filename: &str,
    has_hrrr_ice: bool,
    has_rrfs_ice: bool,
) -> Result<OperationalModel, OperationalRadarGribError> {
    if has_hrrr_ice && has_rrfs_ice {
        return Err(OperationalRadarGribError::Unsupported(
            "both CIMIXR and ICMR are present; operational model dialect is ambiguous".to_owned(),
        ));
    }
    if !has_hrrr_ice && !has_rrfs_ice {
        return Err(OperationalRadarGribError::MissingField(
            "neither HRRR CIMIXR nor RRFS ICMR has a complete native hybrid-level inventory"
                .to_owned(),
        ));
    }
    let filename = filename.to_ascii_lowercase();
    if filename.contains("hrrr") && !has_hrrr_ice {
        return Err(OperationalRadarGribError::Unsupported(
            "filename identifies HRRR but the file carries the RRFS ICMR dialect".to_owned(),
        ));
    }
    if filename.contains("rrfs") && !has_rrfs_ice {
        return Err(OperationalRadarGribError::Unsupported(
            "filename identifies RRFS but the file carries the HRRR CIMIXR dialect".to_owned(),
        ));
    }
    Ok(if has_rrfs_ice {
        OperationalModel::Rrfs
    } else {
        OperationalModel::Hrrr
    })
}

fn validate_base_shapes(
    base: &OperationalRadarBaseFields,
    expected_3d: usize,
) -> Result<(), OperationalRadarGribError> {
    let expected_2d = base.nx * base.ny;
    for (name, values) in [
        ("height", &base.height_msl_m),
        ("temperature", &base.temperature_k),
        ("air density", &base.air_density_kgm3),
        ("u wind", &base.u_mps),
        ("v wind", &base.v_mps),
        ("w wind", &base.w_mps),
    ] {
        if values.len() != expected_3d {
            return Err(OperationalRadarGribError::Shape(format!(
                "{name} has {} cells; expected {expected_3d}",
                values.len()
            )));
        }
    }
    if base
        .tke_m2s2
        .as_ref()
        .is_some_and(|values| values.len() != expected_3d)
    {
        return Err(OperationalRadarGribError::Shape(
            "TKE grid does not match the native three-dimensional grid".to_owned(),
        ));
    }
    for (name, values) in [
        ("latitude", &base.lat),
        ("longitude", &base.lon),
        ("terrain", &base.terrain_m),
    ] {
        if values.len() != expected_2d {
            return Err(OperationalRadarGribError::Shape(format!(
                "{name} has {} cells; expected {expected_2d}",
                values.len()
            )));
        }
    }
    Ok(())
}

fn validate_species_shape(
    species: &OperationalSpeciesField,
    expected: usize,
) -> Result<(), OperationalRadarGribError> {
    if species.mixing_ratio_kgkg.len() != expected
        || species
            .number_per_kg
            .as_ref()
            .is_some_and(|values| values.len() != expected)
    {
        return Err(OperationalRadarGribError::Shape(format!(
            "{} scattering input does not match the {expected}-cell native grid",
            species.kind.label()
        )));
    }
    Ok(())
}

fn merge_linear_scattering(
    existing: crate::wrf_radar_physics::BulkContribution,
    additional: crate::wrf_radar_physics::BulkContribution,
) -> crate::wrf_radar_physics::BulkContribution {
    let mut accumulator = crate::wrf_radar_physics::PolarAccumulator::default();
    accumulator.add(1.0, existing);
    accumulator.add(1.0, additional);
    let combined = accumulator.finalize();
    crate::wrf_radar_physics::BulkContribution {
        zh: combined.zh,
        zv: combined.zv,
        cov_re: combined.cov_re,
        cov_im: combined.cov_im,
        kdp_deg_km: combined.kdp_deg_km,
        ah_db_km: combined.ah_db_km,
        av_db_km: combined.av_db_km,
        fall_speed_mps: combined.fall_speed_mps,
        fall_speed_variance_m2s2: combined.fall_speed_variance_m2s2,
    }
}

fn specific_humidity_to_mixing_ratio(specific_humidity: f32) -> f32 {
    if !specific_humidity.is_finite() || !(0.0..1.0).contains(&specific_humidity) {
        f32::NAN
    } else {
        specific_humidity / (1.0 - specific_humidity)
    }
}

fn omega_to_geometric_vertical_velocity(
    omega_pa_s: &[f32],
    air_density_kgm3: &[f32],
) -> Result<Vec<f32>, OperationalRadarGribError> {
    if omega_pa_s.len() != air_density_kgm3.len() {
        return Err(OperationalRadarGribError::Shape(format!(
            "VVEL carries {} cells but air density carries {}",
            omega_pa_s.len(),
            air_density_kgm3.len()
        )));
    }
    Ok(omega_pa_s
        .iter()
        .zip(air_density_kgm3)
        .map(|(&omega, &density)| {
            if omega.is_finite() && density.is_finite() && density > 0.0 {
                -omega / (density * GRAVITY_MPS2)
            } else {
                f32::NAN
            }
        })
        .collect())
}

fn rotate_grid_relative_winds(
    u_mps: &mut [f32],
    v_mps: &mut [f32],
    lat: &[f32],
    lon: &[f32],
    nx: usize,
    ny: usize,
    nz: usize,
) -> Result<(), OperationalRadarGribError> {
    let plane = nx.checked_mul(ny).ok_or_else(|| {
        OperationalRadarGribError::Shape("wind grid dimensions overflow address space".to_owned())
    })?;
    let expected = plane.checked_mul(nz).ok_or_else(|| {
        OperationalRadarGribError::Shape("wind grid dimensions overflow address space".to_owned())
    })?;
    if nx < 2 || ny == 0 || lat.len() != plane || lon.len() != plane {
        return Err(OperationalRadarGribError::Shape(
            "cannot derive grid-wind orientation from the cropped latitude/longitude grid"
                .to_owned(),
        ));
    }
    if u_mps.len() != expected || v_mps.len() != expected {
        return Err(OperationalRadarGribError::Shape(format!(
            "wind component size does not match {nx}x{ny}x{nz} native grid"
        )));
    }

    let mut rotation = Vec::with_capacity(plane);
    for j in 0..ny {
        for i in 0..nx {
            let (from_i, to_i) = if i + 1 < nx { (i, i + 1) } else { (i - 1, i) };
            let from = j * nx + from_i;
            let to = j * nx + to_i;
            let bearing = initial_bearing_rad(
                f64::from(lat[from]),
                f64::from(lon[from]),
                f64::from(lat[to]),
                f64::from(lon[to]),
            )
            .ok_or_else(|| {
                OperationalRadarGribError::Shape(format!(
                    "cannot derive grid-wind orientation at row {j}, column {i}"
                ))
            })?;
            // Bearing is clockwise from north; gamma is counter-clockwise from
            // true east to the positive grid-x axis.
            let gamma = std::f64::consts::FRAC_PI_2 - bearing;
            rotation.push((gamma.cos() as f32, gamma.sin() as f32));
        }
    }

    for index in 0..expected {
        let (cos_gamma, sin_gamma) = rotation[index % plane];
        let u_grid = u_mps[index];
        let v_grid = v_mps[index];
        u_mps[index] = u_grid * cos_gamma - v_grid * sin_gamma;
        v_mps[index] = u_grid * sin_gamma + v_grid * cos_gamma;
    }
    Ok(())
}

fn initial_bearing_rad(lat_a: f64, lon_a: f64, lat_b: f64, lon_b: f64) -> Option<f64> {
    if !lat_a.is_finite() || !lon_a.is_finite() || !lat_b.is_finite() || !lon_b.is_finite() {
        return None;
    }
    let lat_a = lat_a.to_radians();
    let lat_b = lat_b.to_radians();
    let dlon = normalize_lon(lon_b - lon_a).to_radians();
    let east = dlon.sin() * lat_b.cos();
    let north = lat_a.cos() * lat_b.sin() - lat_a.sin() * lat_b.cos() * dlon.cos();
    (east.abs() + north.abs() > 1.0e-12).then(|| east.atan2(north))
}

fn build_catalog(path: &Path) -> Result<OperationalCatalog, OperationalRadarGribError> {
    let mut file = File::open(path)?;
    let total_len = file.metadata()?.len();
    let locations = index_grib_messages(&mut file, total_len)?;
    if locations.is_empty() {
        return Err(OperationalRadarGribError::Grib(format!(
            "{} contains no complete GRIB2 messages",
            path.display()
        )));
    }
    let mut entries = Vec::with_capacity(locations.len());
    let mut grid = None;
    let mut reference_time = None;
    let mut forecast_time = 0;
    let mut time_range_unit = 1;
    let mut buffer = Vec::new();
    for (location_index, location) in locations.iter().copied().enumerate() {
        read_message_bytes(&mut file, location, &mut buffer)?;
        let parsed = Grib2File::from_bytes(&buffer).map_err(|error| {
            OperationalRadarGribError::Grib(format!(
                "message {} metadata: {error}",
                location_index + 1
            ))
        })?;
        for (submessage_index, message) in parsed.messages.iter().enumerate() {
            match &grid {
                None => {
                    if message.grid.scan_mode != REQUIRED_SCAN_MODE {
                        return Err(OperationalRadarGribError::Shape(format!(
                            "scan mode 0x{:02x}; native HRRR/RRFS radar input requires 0x40",
                            message.grid.scan_mode
                        )));
                    }
                    grid = Some(message.grid.clone());
                    reference_time = Some(message.reference_time);
                    forecast_time = message.product.forecast_time;
                    time_range_unit = message.product.time_range_unit;
                }
                Some(first)
                    if message.grid.nx != first.nx
                        || message.grid.ny != first.ny
                        || message.grid.scan_mode != first.scan_mode
                        || message.grid.template != first.template
                        || message.grid.resolution_flags != first.resolution_flags =>
                {
                    return Err(OperationalRadarGribError::Shape(format!(
                        "message {}.{} changes the file grid",
                        location_index + 1,
                        submessage_index + 1
                    )));
                }
                Some(_) => {
                    if reference_time != Some(message.reference_time)
                        || forecast_time != message.product.forecast_time
                        || time_range_unit != message.product.time_range_unit
                    {
                        return Err(OperationalRadarGribError::Unsupported(format!(
                            "message {}.{} changes the forecast valid time",
                            location_index + 1,
                            submessage_index + 1
                        )));
                    }
                }
            }
            entries.push(CatalogEntry {
                location_index,
                submessage_index,
                code: FieldCode {
                    discipline: message.discipline,
                    category: message.product.parameter_category,
                    number: message.product.parameter_number,
                },
                level_type: message.product.level_type,
                level: message.product.level_value.round().max(0.0) as u32,
            });
        }
    }
    Ok(OperationalCatalog {
        locations,
        entries,
        grid: grid.expect("a non-empty GRIB catalog has a grid"),
        reference_time: reference_time.expect("a non-empty GRIB catalog has a time"),
        forecast_time,
        time_range_unit,
    })
}

fn catalog_valid_time(
    catalog: &OperationalCatalog,
) -> Result<DateTime<Utc>, OperationalRadarGribError> {
    let delta = match catalog.time_range_unit {
        0 => Duration::minutes(i64::from(catalog.forecast_time)),
        1 => Duration::hours(i64::from(catalog.forecast_time)),
        2 => Duration::days(i64::from(catalog.forecast_time)),
        other => {
            return Err(OperationalRadarGribError::Unsupported(format!(
                "forecast time-range unit {other}"
            )));
        }
    };
    Ok(DateTime::from_naive_utc_and_offset(
        catalog.reference_time + delta,
        Utc,
    ))
}

fn read_message_bytes(
    file: &mut File,
    location: MessageLocation,
    buffer: &mut Vec<u8>,
) -> std::io::Result<()> {
    buffer.clear();
    buffer.resize(location.length as usize, 0);
    file.seek(SeekFrom::Start(location.offset))?;
    file.read_exact(buffer)
}

fn decode_entry(
    file: &mut File,
    catalog: &OperationalCatalog,
    entry: CatalogEntry,
    buffer: &mut Vec<u8>,
) -> Result<Vec<f64>, OperationalRadarGribError> {
    let location = catalog.locations[entry.location_index];
    read_message_bytes(file, location, buffer)?;
    let parsed = Grib2File::from_bytes(buffer).map_err(|error| {
        OperationalRadarGribError::Grib(format!("message {}: {error}", entry.location_index + 1))
    })?;
    let message = parsed.messages.get(entry.submessage_index).ok_or_else(|| {
        OperationalRadarGribError::Grib(format!(
            "message {} lost submessage {} on decode",
            entry.location_index + 1,
            entry.submessage_index + 1
        ))
    })?;
    let values = unpack_message(message).map_err(|error| {
        OperationalRadarGribError::Grib(format!(
            "message {} field decode: {error}",
            entry.location_index + 1
        ))
    })?;
    let expected = catalog.grid.nx as usize * catalog.grid.ny as usize;
    if values.len() != expected {
        return Err(OperationalRadarGribError::Shape(format!(
            "decoded {} values for a {expected}-cell plane",
            values.len()
        )));
    }
    Ok(values)
}

fn hybrid_entries(catalog: &OperationalCatalog, code: FieldCode) -> BTreeMap<u32, CatalogEntry> {
    let mut entries = BTreeMap::new();
    for entry in &catalog.entries {
        if entry.code == code && entry.level_type == LEVEL_HYBRID && entry.level > 0 {
            entries.entry(entry.level).or_insert(*entry);
        }
    }
    entries
}

fn validate_levels(
    levels: impl IntoIterator<Item = u32>,
    field: &str,
    expected: usize,
) -> Result<(), OperationalRadarGribError> {
    let levels = levels.into_iter().collect::<Vec<_>>();
    if levels.len() != expected {
        return Err(OperationalRadarGribError::Shape(format!(
            "{field} has {} hybrid levels; expected {expected}",
            levels.len()
        )));
    }
    for (index, level) in levels.into_iter().enumerate() {
        let wanted = index as u32 + 1;
        if level != wanted {
            return Err(OperationalRadarGribError::Shape(format!(
                "{field} hybrid levels are not contiguous: expected {wanted}, found {level}"
            )));
        }
    }
    Ok(())
}

fn complete_level_count(
    catalog: &OperationalCatalog,
    code: FieldCode,
    field: &str,
) -> Result<usize, OperationalRadarGribError> {
    let entries = hybrid_entries(catalog, code);
    if entries.is_empty() {
        return Err(OperationalRadarGribError::MissingField(format!(
            "{field} has no hybrid-level messages"
        )));
    }
    let count = entries.len();
    validate_levels(entries.keys().copied(), field, count)?;
    Ok(count)
}

fn has_complete(catalog: &OperationalCatalog, code: FieldCode, expected: usize) -> bool {
    let entries = hybrid_entries(catalog, code);
    validate_levels(entries.keys().copied(), "optional field", expected).is_ok()
}

fn latlon_planes(grid: &GridDefinition) -> Result<(Vec<f32>, Vec<f32>), OperationalRadarGribError> {
    let (lat, lon) = grid_latlon(grid);
    let expected = grid.nx as usize * grid.ny as usize;
    if lat.len() != expected || lon.len() != expected {
        return Err(OperationalRadarGribError::Shape(format!(
            "grid template 3.{} did not produce coordinates",
            grid.template
        )));
    }
    Ok((
        lat.into_iter().map(|value| value as f32).collect(),
        lon.into_iter()
            .map(|value| normalize_lon(value) as f32)
            .collect(),
    ))
}

fn crop_plane(values: &[f32], source_nx: usize, crop: RadarCrop) -> Vec<f32> {
    let mut cropped = Vec::with_capacity(crop.nx() * crop.ny());
    for row in crop.j0..=crop.j1 {
        let start = row * source_nx + crop.i0;
        cropped.extend_from_slice(&values[start..start + crop.nx()]);
    }
    cropped
}

fn crop_plane_f64(values: &[f64], source_nx: usize, crop: RadarCrop) -> Vec<f32> {
    let mut cropped = Vec::with_capacity(crop.nx() * crop.ny());
    for row in crop.j0..=crop.j1 {
        let start = row * source_nx + crop.i0;
        cropped.extend(
            values[start..start + crop.nx()]
                .iter()
                .map(|value| *value as f32),
        );
    }
    cropped
}

fn normalize_lon(lon: f64) -> f64 {
    (lon + 180.0).rem_euclid(360.0) - 180.0
}

fn operational_grid_spacing_m(template: u16, dx: f64) -> Option<f64> {
    // HRRR and CONUS RRFS native files are projected grids whose template
    // stores Dx in metres.  Do not reinterpret angular spacing from regular or
    // rotated latitude/longitude templates as metres.
    matches!(template, 10 | 20 | 30)
        .then_some(dx)
        .filter(|value| value.is_finite() && *value > 0.0)
}

fn great_circle_distance_km(lat_a: f64, lon_a: f64, lat_b: f64, lon_b: f64) -> f64 {
    let dlat = (lat_b - lat_a).to_radians();
    let dlon = normalize_lon(lon_b - lon_a).to_radians();
    let lat_a = lat_a.to_radians();
    let lat_b = lat_b.to_radians();
    let haversine =
        (dlat * 0.5).sin().powi(2) + lat_a.cos() * lat_b.cos() * (dlon * 0.5).sin().powi(2);
    2.0 * 6_371.0 * haversine.clamp(0.0, 1.0).sqrt().asin()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radar_crop_keeps_range_hull_and_margin() {
        let nx = 5;
        let ny = 5;
        let mut lat = Vec::new();
        let mut lon = Vec::new();
        for j in 0..ny {
            for i in 0..nx {
                lat.push(35.0 + j as f32);
                lon.push(-99.0 + i as f32);
            }
        }
        let crop = RadarCrop::around_radar(&lat, &lon, nx, ny, 37.0, -97.0, 10.0).unwrap();
        assert!(crop.i0 <= 2 && crop.i1 >= 2);
        assert!(crop.j0 <= 2 && crop.j1 >= 2);
        assert!(crop.nx() >= 1 && crop.ny() >= 1);
    }

    #[test]
    fn radar_crop_rejects_site_outside_domain() {
        let lat = vec![35.0; 4];
        let lon = vec![-97.0; 4];
        let error = RadarCrop::around_radar(&lat, &lon, 2, 2, -40.0, 120.0, 10.0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("outside the model domain"), "{error}");
    }

    #[test]
    fn crop_plane_preserves_level_row_order() {
        let values = (0..20).map(|value| value as f32).collect::<Vec<_>>();
        let crop = RadarCrop {
            i0: 1,
            i1: 3,
            j0: 1,
            j1: 2,
        };
        assert_eq!(
            crop_plane(&values, 5, crop),
            vec![6.0, 7.0, 8.0, 11.0, 12.0, 13.0]
        );
    }

    #[test]
    fn valid_level_contract_rejects_gaps() {
        assert!(validate_levels([1, 2, 3], "TMP", 3).is_ok());
        assert!(validate_levels([1, 3], "TMP", 2).is_err());
        assert!(validate_levels([1, 2], "TMP", 3).is_err());
    }

    #[test]
    fn species_codes_follow_hrrr_and_rrfs_ice_dialects() {
        assert_eq!(
            species_mass_code(OperationalHydrometeor::CloudIce, OperationalModel::Hrrr),
            Some(CODE_CIMIXR)
        );
        assert_eq!(
            species_mass_code(OperationalHydrometeor::CloudIce, OperationalModel::Rrfs),
            Some(CODE_ICMR)
        );
        assert_eq!(species_number_code(OperationalHydrometeor::Graupel), None);
    }

    #[test]
    fn model_dialect_detection_fails_closed_on_ambiguity_and_conflict() {
        assert_eq!(
            classify_operational_model("hrrr.t12z.wrfnatf00.grib2", true, false).unwrap(),
            OperationalModel::Hrrr
        );
        assert_eq!(
            classify_operational_model("rrfs.t12z.nativef00.grib2", false, true).unwrap(),
            OperationalModel::Rrfs
        );
        assert!(classify_operational_model("native.grib2", true, true).is_err());
        assert!(classify_operational_model("native.grib2", false, false).is_err());
        assert!(classify_operational_model("hrrr.t12z.wrfnatf00.grib2", false, true).is_err());
    }

    #[test]
    fn operational_scheme_ids_state_the_assumption() {
        assert!(
            OperationalSchemeAssumption::HrrrThompsonCategoryBulkV1
                .stable_id()
                .contains("thompson")
        );
        let rrfs = OperationalSchemeAssumption::RrfsCategoryBulkV1.description();
        assert!(rrfs.contains("no unencoded scheme identity is inferred"));
    }

    #[test]
    fn additive_scattering_merge_stays_in_linear_space() {
        let first = crate::wrf_radar_physics::BulkContribution {
            zh: 10.0,
            zv: 5.0,
            cov_re: 4.0,
            kdp_deg_km: 0.2,
            fall_speed_mps: 2.0,
            ..Default::default()
        };
        let second = crate::wrf_radar_physics::BulkContribution {
            zh: 30.0,
            zv: 15.0,
            cov_re: 12.0,
            kdp_deg_km: 0.7,
            fall_speed_mps: 6.0,
            ..Default::default()
        };
        let merged = merge_linear_scattering(first, second);
        assert!((merged.zh - 40.0).abs() < 1.0e-6);
        assert!((merged.zv - 20.0).abs() < 1.0e-6);
        assert!((merged.cov_re - 16.0).abs() < 1.0e-6);
        assert!((merged.kdp_deg_km - 0.9).abs() < 1.0e-6);
        assert!((merged.fall_speed_mps - 5.0).abs() < 1.0e-6);
    }

    #[test]
    fn spfh_is_converted_to_water_vapor_mixing_ratio() {
        assert!((specific_humidity_to_mixing_ratio(0.01) - 0.010_101_01).abs() < 1.0e-7);
        assert!(specific_humidity_to_mixing_ratio(1.0).is_nan());
        assert!(specific_humidity_to_mixing_ratio(-0.1).is_nan());
    }

    #[test]
    fn pressure_velocity_is_not_mislabeled_as_geometric_w() {
        let w = omega_to_geometric_vertical_velocity(&[-9.806_65], &[1.0]).unwrap();
        assert!((w[0] - 1.0).abs() < 1.0e-6);
        assert!(omega_to_geometric_vertical_velocity(&[1.0], &[1.0, 2.0]).is_err());
    }

    #[test]
    fn grid_relative_winds_rotate_to_earth_axes() {
        // Positive grid x points north on this synthetic grid. A +x wind must
        // therefore become earth-relative +v rather than +u.
        let lat = vec![35.0, 36.0, 35.0, 36.0];
        let lon = vec![-97.0; 4];
        let mut u = vec![10.0; 4];
        let mut v = vec![0.0; 4];
        rotate_grid_relative_winds(&mut u, &mut v, &lat, &lon, 2, 2, 1).unwrap();
        for value in u {
            assert!(value.abs() < 1.0e-4, "earth u was {value}");
        }
        for value in v {
            assert!((value - 10.0).abs() < 1.0e-4, "earth v was {value}");
        }
    }

    #[test]
    fn great_circle_distance_handles_date_line() {
        let distance = great_circle_distance_km(10.0, 179.8, 10.0, -179.8);
        assert!(distance < 50.0, "date-line distance was {distance} km");
    }

    #[test]
    fn projected_grid_spacing_is_exposed_without_relabeling_degrees() {
        assert_eq!(operational_grid_spacing_m(30, 3_000.0), Some(3_000.0));
        assert_eq!(operational_grid_spacing_m(0, 0.025), None);
        assert_eq!(operational_grid_spacing_m(30, f64::NAN), None);
    }
}
