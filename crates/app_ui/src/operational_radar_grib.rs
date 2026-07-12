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

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use grib_core::grib2::{Grib2File, GridDefinition, grid_latlon, unpack_message};
use simsat::ingest_grib::{
    CODE_CIMIXR, CODE_CLMR, CODE_GRLE, CODE_HGT, CODE_ICMR, CODE_NCCICE, CODE_NCONCD, CODE_PRES,
    CODE_RWMR, CODE_SNMR, CODE_SPFH, CODE_SPNCR, CODE_TMP, CODE_UGRD, CODE_VGRD, FieldCode,
    LEVEL_HYBRID, LEVEL_SURFACE, MessageLocation, REQUIRED_SCAN_MODE, index_grib_messages,
};

const CODE_W: FieldCode = FieldCode {
    discipline: 0,
    category: 2,
    number: 8,
};
const CODE_TKE: FieldCode = FieldCode {
    discipline: 0,
    category: 19,
    number: 11,
};

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

impl OperationalRadarGribReader {
    pub fn probe(path: &Path) -> Result<OperationalRadarProbe, OperationalRadarGribError> {
        let catalog = build_catalog(path)?;
        let nz = complete_level_count(&catalog, CODE_TMP, "TMP")?;
        let model = detect_operational_model(path, &catalog);
        let available_species = OperationalHydrometeor::ALL
            .into_iter()
            .filter(|kind| {
                species_mass_code(*kind, model).is_some_and(|code| has_complete(&catalog, code, nz))
            })
            .collect();
        Ok(OperationalRadarProbe {
            model,
            nx: catalog.grid.nx as usize,
            ny: catalog.grid.ny as usize,
            nz,
            valid_time: catalog_valid_time(&catalog)?,
            available_species,
            has_vertical_velocity: has_complete(&catalog, CODE_W, nz),
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
        let model = detect_operational_model(path, &catalog);
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
            *density =
                crate::wrf_radar_physics::air_density(pressure, temperature, specific_humidity);
        }
        progress("reading native model wind");
        let u_mps = self.read_required_hybrid(CODE_UGRD, "UGRD")?;
        let v_mps = self.read_required_hybrid(CODE_VGRD, "VGRD")?;
        let w_mps = self
            .read_optional_hybrid(CODE_W, "VVEL")?
            .unwrap_or_else(|| vec![0.0; height_msl_m.len()]);
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

fn detect_operational_model(path: &Path, catalog: &OperationalCatalog) -> OperationalModel {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if filename.contains("rrfs") || !hybrid_entries(catalog, CODE_ICMR).is_empty() {
        OperationalModel::Rrfs
    } else {
        OperationalModel::Hrrr
    }
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
                        || message.grid.scan_mode != first.scan_mode =>
                {
                    return Err(OperationalRadarGribError::Shape(format!(
                        "message {}.{} changes the file grid",
                        location_index + 1,
                        submessage_index + 1
                    )));
                }
                Some(_) => {}
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
    fn great_circle_distance_handles_date_line() {
        let distance = great_circle_distance_km(10.0, 179.8, 10.0, -179.8);
        assert!(distance < 50.0, "date-line distance was {distance} km");
    }
}
