//! Core data model for the clean-room Rust radar analyst.
//!
//! The model is intentionally data-oriented: radial geometry lives beside compact
//! moment arrays so decoders, product algorithms, and GPU upload code can share a
//! stable contract without per-gate heap objects.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A NEXRAD, TDWR, or compatible radar site.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RadarSite {
    pub id: String,
    pub name: Option<String>,
    pub latitude_deg: Option<f32>,
    pub longitude_deg: Option<f32>,
    pub elevation_m: Option<f32>,
}

impl RadarSite {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
            latitude_deg: None,
            longitude_deg: None,
            elevation_m: None,
        }
    }
}

/// Decoded radar volume with raw moments grouped by elevation cut.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RadarVolume {
    pub site: RadarSite,
    pub volume_time: DateTime<Utc>,
    pub vcp: Option<VcpInfo>,
    pub cuts: Vec<ElevationCut>,
    pub metadata: VolumeMetadata,
}

impl RadarVolume {
    pub fn new(site: RadarSite, volume_time: DateTime<Utc>) -> Self {
        Self {
            site,
            volume_time,
            vcp: None,
            cuts: Vec::new(),
            metadata: VolumeMetadata::default(),
        }
    }

    pub fn find_or_insert_cut(
        &mut self,
        elevation_deg: f32,
        elevation_number: Option<u8>,
    ) -> &mut ElevationCut {
        if let Some(index) = self.cuts.iter().rposition(|cut| {
            cut.elevation_number == elevation_number
                || (cut.elevation_deg - elevation_deg).abs() <= CUT_ELEVATION_MATCH_TOLERANCE_DEG
        }) {
            return &mut self.cuts[index];
        }

        self.push_cut(elevation_deg, elevation_number)
    }

    pub fn push_cut(
        &mut self,
        elevation_deg: f32,
        elevation_number: Option<u8>,
    ) -> &mut ElevationCut {
        self.cuts
            .push(ElevationCut::new(elevation_deg, elevation_number));
        self.cuts.last_mut().expect("cut was just inserted")
    }
}

impl Default for RadarVolume {
    fn default() -> Self {
        Self::new(
            RadarSite::new(""),
            DateTime::<Utc>::from_timestamp(0, 0).expect("unix epoch is a valid timestamp"),
        )
    }
}

/// Tolerance used to treat two cut elevations as the same tilt, both by
/// [`RadarVolume::find_or_insert_cut`] and by [`merge_radar_volumes`].
pub const CUT_ELEVATION_MATCH_TOLERANCE_DEG: f32 = 0.05;

/// Counters describing what [`merge_radar_volumes`] did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct MergeReport {
    /// Moment grids moved from a later part into an elevation-matched cut.
    pub merged_moments: usize,
    /// Later-part cuts that matched an elevation but had different
    /// radial/gate geometry and were therefore dropped entirely.
    pub skipped_geometry: usize,
    /// Moment grids dropped because the matched cut already carried that
    /// moment type (first part wins).
    pub moment_collisions: usize,
}

/// Merge per-product / per-sweep partial volumes of one scan into a single
/// [`RadarVolume`].
///
/// Several international feeds split one physical volume scan across files:
/// ODIM_H5 feeds (EUMETNET OPERA Data Information Model; Michelson et al.,
/// OPERA WP 2.1/2.2, v2.2-2.3) like DWD/CHMI publish one file per product
/// and/or sweep, SHMU one full PVOL per product. This helper reassembles
/// them.
///
/// Semantics:
/// - All parts must share `site.id`, else `Err`. The first part supplies the
///   site record, VCP, and metadata.
/// - `volume_time` is the earliest part time (parts of one scan carry
///   per-product write times).
/// - Cuts are matched by `elevation_deg` within
///   [`CUT_ELEVATION_MATCH_TOLERANCE_DEG`]; an incoming cut merges into the
///   first existing cut inside the tolerance.
/// - A matched cut contributes its moment grids to the existing cut's
///   moment map. On a moment-type collision the first part wins and the
///   incoming grid is dropped (counted in
///   [`MergeReport::moment_collisions`]); otherwise the move is counted in
///   [`MergeReport::merged_moments`].
/// - A matched cut whose radial geometry differs (radial count or per-radial
///   azimuth beyond the same tolerance) is NOT merged: moment-grid rows index
///   into the cut's radial list, so mixing azimuths would scramble the
///   display. Different gate ranges are allowed because every [`MomentGrid`]
///   carries its own [`GateRange`] and render/readout code uses the selected
///   grid's range. Rejected cuts are counted in [`MergeReport::skipped_geometry`].
/// - Unmatched cuts are unioned in, and the final cut list is sorted by
///   elevation (stable: equal elevations keep first-part-then-arrival
///   order).
///
/// Never panics: malformed combinations come back as `Err` or as skip
/// counters.
pub fn merge_radar_volumes(parts: Vec<RadarVolume>) -> Result<(RadarVolume, MergeReport), String> {
    let mut parts = parts.into_iter();
    let Some(mut base) = parts.next() else {
        return Err("no radar volumes to merge".to_owned());
    };
    let mut report = MergeReport::default();

    for part in parts {
        if part.site.id != base.site.id {
            return Err(format!(
                "cannot merge radar volumes from different sites: '{}' vs '{}'",
                base.site.id, part.site.id
            ));
        }
        if part.volume_time < base.volume_time {
            base.volume_time = part.volume_time;
        }
        for cut in part.cuts {
            let matched = base.cuts.iter_mut().find(|existing| {
                (existing.elevation_deg - cut.elevation_deg).abs()
                    <= CUT_ELEVATION_MATCH_TOLERANCE_DEG
            });
            let Some(existing) = matched else {
                base.cuts.push(cut);
                continue;
            };
            if !cut_radials_match(existing, &cut) {
                report.skipped_geometry += 1;
                continue;
            }
            merge_radial_metadata(existing, &cut);
            for (moment, grid) in cut.moments {
                match existing.moments.entry(moment) {
                    std::collections::btree_map::Entry::Vacant(slot) => {
                        slot.insert(grid);
                        report.merged_moments += 1;
                    }
                    std::collections::btree_map::Entry::Occupied(_) => {
                        report.moment_collisions += 1;
                    }
                }
            }
        }
    }

    base.cuts
        .sort_by(|a, b| a.elevation_deg.total_cmp(&b.elevation_deg));
    Ok((base, report))
}

/// `true` when two cuts describe the same radial azimuth geometry, so moment
/// grids whose rows index one cut's radials are valid against the other's.
fn cut_radials_match(a: &ElevationCut, b: &ElevationCut) -> bool {
    a.radials.len() == b.radials.len()
        && a.radials.iter().zip(&b.radials).all(|(ra, rb)| {
            azimuth_difference_deg(ra.azimuth_deg, rb.azimuth_deg)
                <= CUT_ELEVATION_MATCH_TOLERANCE_DEG
        })
}

fn merge_radial_metadata(existing: &mut ElevationCut, incoming: &ElevationCut) {
    for (base, other) in existing.radials.iter_mut().zip(&incoming.radials) {
        if base.nyquist_velocity_mps.is_none() {
            base.nyquist_velocity_mps = other.nyquist_velocity_mps;
        }
    }
}

/// Smallest absolute angular difference, wrap-aware (359.99° vs 0.01° is
/// 0.02°, not 359.98°).
fn azimuth_difference_deg(a: f32, b: f32) -> f32 {
    let diff = (a - b).abs() % 360.0;
    diff.min(360.0 - diff)
}

/// One elevation sweep/cut in a volume scan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ElevationCut {
    pub elevation_deg: f32,
    pub elevation_number: Option<u8>,
    pub radials: Vec<Radial>,
    pub moments: BTreeMap<MomentType, MomentGrid>,
}

impl ElevationCut {
    pub fn new(elevation_deg: f32, elevation_number: Option<u8>) -> Self {
        Self {
            elevation_deg,
            elevation_number,
            radials: Vec::new(),
            moments: BTreeMap::new(),
        }
    }

    pub fn moments_available(&self) -> BTreeSet<MomentType> {
        self.moments.keys().cloned().collect()
    }
}

/// Geometry and timing for one radial.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Radial {
    pub azimuth_deg: f32,
    pub elevation_deg: f32,
    pub time_offset_ms: i32,
    pub gate_range: GateRange,
    pub nyquist_velocity_mps: Option<f32>,
    pub radial_status: Option<RadialStatus>,
}

/// Gate layout for a radial or moment grid.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GateRange {
    pub first_gate_m: i32,
    pub gate_spacing_m: i32,
    pub gate_count: usize,
}

/// NEXRAD radial status markers used to detect sweep and volume boundaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RadialStatus {
    StartElevation,
    Intermediate,
    EndElevation,
    StartVolume,
    EndVolume,
    StartElevationLastCut,
    Unknown(u8),
}

impl From<u8> for RadialStatus {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::StartElevation,
            1 => Self::Intermediate,
            2 => Self::EndElevation,
            3 => Self::StartVolume,
            4 => Self::EndVolume,
            5 => Self::StartElevationLastCut,
            other => Self::Unknown(other),
        }
    }
}

/// Base radar moment. Unknown names are preserved for forward compatibility.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub enum MomentType {
    Reflectivity,
    Velocity,
    SpectrumWidth,
    DifferentialReflectivity,
    CorrelationCoefficient,
    DifferentialPhase,
    SpecificDifferentialPhase,
    Unknown(String),
}

impl MomentType {
    pub fn from_nexrad_name(name: &str) -> Self {
        match name.trim() {
            "REF" => Self::Reflectivity,
            "VEL" => Self::Velocity,
            "SW" => Self::SpectrumWidth,
            "ZDR" => Self::DifferentialReflectivity,
            "RHO" => Self::CorrelationCoefficient,
            "PHI" => Self::DifferentialPhase,
            "KDP" => Self::SpecificDifferentialPhase,
            other => Self::Unknown(other.to_owned()),
        }
    }

    pub fn from_nexrad_bytes(name: &[u8]) -> Self {
        match name {
            b"REF" => return Self::Reflectivity,
            b"VEL" => return Self::Velocity,
            b"SW " | b"SW" => return Self::SpectrumWidth,
            b"ZDR" => return Self::DifferentialReflectivity,
            b"RHO" => return Self::CorrelationCoefficient,
            b"PHI" => return Self::DifferentialPhase,
            b"KDP" => return Self::SpecificDifferentialPhase,
            _ => {}
        }

        match trim_ascii_name(name) {
            b"REF" => Self::Reflectivity,
            b"VEL" => Self::Velocity,
            b"SW" => Self::SpectrumWidth,
            b"ZDR" => Self::DifferentialReflectivity,
            b"RHO" => Self::CorrelationCoefficient,
            b"PHI" => Self::DifferentialPhase,
            b"KDP" => Self::SpecificDifferentialPhase,
            other => Self::Unknown(String::from_utf8_lossy(other).into_owned()),
        }
    }

    pub fn short_name(&self) -> &str {
        match self {
            Self::Reflectivity => "REF",
            Self::Velocity => "VEL",
            Self::SpectrumWidth => "SW",
            Self::DifferentialReflectivity => "ZDR",
            Self::CorrelationCoefficient => "RHO",
            Self::DifferentialPhase => "PHI",
            Self::SpecificDifferentialPhase => "KDP",
            Self::Unknown(name) => name.as_str(),
        }
    }
}

fn trim_ascii_name(mut bytes: &[u8]) -> &[u8] {
    while matches!(bytes.first(), Some(0 | b' ' | b'\t' | b'\r' | b'\n')) {
        bytes = &bytes[1..];
    }
    while matches!(bytes.last(), Some(0 | b' ' | b'\t' | b'\r' | b'\n')) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

impl fmt::Display for MomentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.short_name())
    }
}

/// Product identifier used by future base and derived-product registries.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub struct ProductId(pub String);

impl From<MomentType> for ProductId {
    fn from(moment: MomentType) -> Self {
        Self(moment.short_name().to_owned())
    }
}

/// Compact moment grid for one sweep. Rows are linked back to radial indices.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MomentGrid {
    pub moment: MomentType,
    pub gate_range: GateRange,
    pub scale: f32,
    pub offset: f32,
    pub nodata: Option<u16>,
    pub range_folded: Option<u16>,
    pub radial_indices: Vec<usize>,
    pub storage: MomentStorage,
}

impl MomentGrid {
    pub fn new_u8(
        moment: MomentType,
        gate_range: GateRange,
        scale: f32,
        offset: f32,
        nodata: Option<u8>,
        range_folded: Option<u8>,
    ) -> Self {
        Self {
            moment,
            gate_range,
            scale,
            offset,
            nodata: nodata.map(u16::from),
            range_folded: range_folded.map(u16::from),
            radial_indices: Vec::new(),
            storage: MomentStorage::U8(Vec::new()),
        }
    }

    pub fn new_u16(
        moment: MomentType,
        gate_range: GateRange,
        scale: f32,
        offset: f32,
        nodata: Option<u16>,
        range_folded: Option<u16>,
    ) -> Self {
        Self {
            moment,
            gate_range,
            scale,
            offset,
            nodata,
            range_folded,
            radial_indices: Vec::new(),
            storage: MomentStorage::U16(Vec::new()),
        }
    }

    pub fn radial_count(&self) -> usize {
        self.radial_indices.len()
    }

    pub fn reserve_rows(&mut self, additional_rows: usize) {
        self.radial_indices.reserve(additional_rows);
        let additional_values = additional_rows.saturating_mul(self.gate_range.gate_count);
        match &mut self.storage {
            MomentStorage::U8(values) => values.reserve(additional_values),
            MomentStorage::U16(values) => values.reserve(additional_values),
            MomentStorage::F32(values) => values.reserve(additional_values),
        }
    }

    pub fn push_row(&mut self, radial_index: usize, row: MomentRow) -> Result<(), MomentGridError> {
        if row.len() > self.gate_range.gate_count {
            self.expand_gate_count(row.len());
        }

        match (&mut self.storage, row) {
            (MomentStorage::U8(values), MomentRow::U8(mut row)) => {
                row.resize(self.gate_range.gate_count, self.nodata.unwrap_or(0) as u8);
                values.extend(row);
            }
            (MomentStorage::U16(values), MomentRow::U16(mut row)) => {
                row.resize(self.gate_range.gate_count, self.nodata.unwrap_or(0));
                values.extend(row);
            }
            (MomentStorage::F32(values), MomentRow::F32(mut row)) => {
                row.resize(self.gate_range.gate_count, f32::NAN);
                values.extend(row);
            }
            (storage, row) => {
                return Err(MomentGridError::StorageMismatch {
                    expected: storage.word_size_bits(),
                    actual: row.word_size_bits(),
                });
            }
        }
        self.radial_indices.push(radial_index);
        Ok(())
    }

    pub fn push_u8_row_slice(
        &mut self,
        radial_index: usize,
        row: &[u8],
    ) -> Result<(), MomentGridError> {
        if row.len() > self.gate_range.gate_count {
            self.expand_gate_count(row.len());
        }

        let MomentStorage::U8(values) = &mut self.storage else {
            return Err(MomentGridError::StorageMismatch {
                expected: self.storage.word_size_bits(),
                actual: 8,
            });
        };

        values.extend_from_slice(row);
        if row.len() < self.gate_range.gate_count {
            values.resize(
                values.len() + (self.gate_range.gate_count - row.len()),
                self.nodata.unwrap_or(0) as u8,
            );
        }
        self.radial_indices.push(radial_index);
        Ok(())
    }

    pub fn push_u16_be_row_bytes(
        &mut self,
        radial_index: usize,
        row: &[u8],
    ) -> Result<(), MomentGridError> {
        if !row.len().is_multiple_of(2) {
            return Err(MomentGridError::InvalidRowByteLength {
                word_size_bits: 16,
                byte_len: row.len(),
            });
        }

        let row_gate_count = row.len() / 2;
        if row_gate_count > self.gate_range.gate_count {
            self.expand_gate_count(row_gate_count);
        }

        let expected = self.storage.word_size_bits();
        let MomentStorage::U16(values) = &mut self.storage else {
            return Err(MomentGridError::StorageMismatch {
                expected,
                actual: 16,
            });
        };

        values.extend(
            row.chunks_exact(2)
                .map(|gate| u16::from_be_bytes([gate[0], gate[1]])),
        );
        if row_gate_count < self.gate_range.gate_count {
            values.resize(
                values.len() + (self.gate_range.gate_count - row_gate_count),
                self.nodata.unwrap_or(0),
            );
        }
        self.radial_indices.push(radial_index);
        Ok(())
    }

    pub fn scaled_value(&self, row_index: usize, gate_index: usize) -> Option<f32> {
        if gate_index >= self.gate_range.gate_count {
            return None;
        }

        let index = row_index
            .checked_mul(self.gate_range.gate_count)?
            .checked_add(gate_index)?;

        match &self.storage {
            MomentStorage::U8(values) => {
                let raw = u16::from(*values.get(index)?);
                self.scale_raw(raw)
            }
            MomentStorage::U16(values) => {
                let raw = *values.get(index)?;
                self.scale_raw(raw)
            }
            MomentStorage::F32(values) => values.get(index).copied(),
        }
    }

    fn scale_raw(&self, raw: u16) -> Option<f32> {
        if self.nodata == Some(raw) || self.range_folded == Some(raw) {
            return None;
        }
        Some((raw as f32 - self.offset) / self.scale)
    }

    fn expand_gate_count(&mut self, new_gate_count: usize) {
        let old_gate_count = self.gate_range.gate_count;
        if new_gate_count <= old_gate_count {
            return;
        }

        let rows = self.radial_indices.len();
        if rows == 0 {
            self.gate_range.gate_count = new_gate_count;
            return;
        }

        match &mut self.storage {
            MomentStorage::U8(values) => {
                let fill = self.nodata.unwrap_or(0) as u8;
                *values = expand_rows(values, rows, old_gate_count, new_gate_count, fill);
            }
            MomentStorage::U16(values) => {
                let fill = self.nodata.unwrap_or(0);
                *values = expand_rows(values, rows, old_gate_count, new_gate_count, fill);
            }
            MomentStorage::F32(values) => {
                *values = expand_rows(values, rows, old_gate_count, new_gate_count, f32::NAN);
            }
        }
        self.gate_range.gate_count = new_gate_count;
    }
}

fn expand_rows<T: Copy>(
    values: &[T],
    rows: usize,
    old_gate_count: usize,
    new_gate_count: usize,
    fill: T,
) -> Vec<T> {
    let mut expanded = Vec::with_capacity(rows * new_gate_count);
    for row in values.chunks(old_gate_count).take(rows) {
        expanded.extend_from_slice(row);
        expanded.resize(expanded.len() + (new_gate_count - old_gate_count), fill);
    }
    expanded
}

/// Backing storage for a moment grid.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum MomentStorage {
    U8(Vec<u8>),
    U16(Vec<u16>),
    F32(Vec<f32>),
}

impl MomentStorage {
    pub fn word_size_bits(&self) -> u8 {
        match self {
            Self::U8(_) => 8,
            Self::U16(_) => 16,
            Self::F32(_) => 32,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::U8(values) => values.len(),
            Self::U16(values) => values.len(),
            Self::F32(values) => values.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One decoded row of moment values.
#[derive(Clone, Debug, PartialEq)]
pub enum MomentRow {
    U8(Vec<u8>),
    U16(Vec<u16>),
    F32(Vec<f32>),
}

impl MomentRow {
    pub fn len(&self) -> usize {
        match self {
            Self::U8(values) => values.len(),
            Self::U16(values) => values.len(),
            Self::F32(values) => values.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn word_size_bits(&self) -> u8 {
        match self {
            Self::U8(_) => 8,
            Self::U16(_) => 16,
            Self::F32(_) => 32,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MomentGridError {
    GateCountMismatch { expected: usize, actual: usize },
    StorageMismatch { expected: u8, actual: u8 },
    InvalidRowByteLength { word_size_bits: u8, byte_len: usize },
}

impl fmt::Display for MomentGridError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GateCountMismatch { expected, actual } => {
                write!(f, "gate count mismatch: expected {expected}, got {actual}")
            }
            Self::StorageMismatch { expected, actual } => {
                write!(
                    f,
                    "moment storage mismatch: expected {expected}-bit, got {actual}-bit"
                )
            }
            Self::InvalidRowByteLength {
                word_size_bits,
                byte_len,
            } => {
                write!(
                    f,
                    "{word_size_bits}-bit moment row has invalid byte length {byte_len}"
                )
            }
        }
    }
}

impl Error for MomentGridError {}

/// Volume Coverage Pattern metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VcpInfo {
    pub pattern: u16,
}

/// Antenna scanning strategy declared by the data source.
///
/// NEXRAD Archive II is always PPI surveillance, but mobile/research formats
/// (DORADE, CfRadial, ODIM_H5) carry explicit scan modes: RHI sweeps hold a
/// fixed azimuth and sweep the antenna in elevation, so a plan-view (PPI)
/// renderer shows them as a single spoke — displays must branch on this.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ScanMode {
    /// Plan-position indicator: fixed elevation, azimuth sweep (includes
    /// 360° surveillance and sector scans).
    Ppi,
    /// Range-height indicator: fixed azimuth, elevation sweep.
    Rhi,
    /// Vertically pointing (birdbath calibration / profiling).
    VerticalPointing,
    /// Declared by the source but not one of the modes above (coplane,
    /// manual, idle, calibration, airborne, ...).
    Other,
}

/// Provenance and decode statistics.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct VolumeMetadata {
    pub source_path: Option<String>,
    pub archive_version: Option<String>,
    pub compression: Option<String>,
    pub message_count: usize,
    pub decoded_radial_count: usize,
    pub skipped_message_count: usize,
    /// Scan strategy declared by the source format, when it carries one
    /// (`None` for formats that do not declare it, e.g. Archive II).
    #[serde(default)]
    pub scan_mode: Option<ScanMode>,
}

/// Earth's mean radius (m).
pub const EARTH_RADIUS_M: f64 = 6_371_000.0;
/// Effective Earth radius under the standard "4/3 Earth" refraction model
/// (Bean & Dutton 1968; the standard-atmosphere refractivity gradient).
pub const EFFECTIVE_EARTH_RADIUS_M: f64 = EARTH_RADIUS_M * 4.0 / 3.0;

/// Center height of the radar beam **above the antenna**, in metres, under the
/// 4/3-Earth-radius effective-radius approximation for atmospheric refraction.
///
/// Doviak & Zrnić (1993), *Doppler Radar and Weather Observations* (2nd ed.),
/// eq. 2.28b: `h = sqrt(r² + aₑ² + 2·r·aₑ·sin θ) − aₑ`, with `aₑ = 4/3·a`.
///
/// `slant_range_m` is range along the beam; `elevation_deg` the antenna
/// elevation angle. Add the antenna's MSL altitude to get beam MSL height.
pub fn beam_height_above_radar_m(slant_range_m: f64, elevation_deg: f64) -> f64 {
    let ae = EFFECTIVE_EARTH_RADIUS_M;
    let r = slant_range_m;
    let theta = elevation_deg.to_radians();
    (r * r + ae * ae + 2.0 * r * ae * theta.sin()).sqrt() - ae
}

/// Great-circle (ground) distance from the radar to the gate, in metres, under
/// the same 4/3-Earth model. Doviak & Zrnić (1993) eq. 2.28c.
pub fn beam_ground_range_m(slant_range_m: f64, elevation_deg: f64) -> f64 {
    let ae = EFFECTIVE_EARTH_RADIUS_M;
    let r = slant_range_m;
    let theta = elevation_deg.to_radians();
    let h = beam_height_above_radar_m(r, elevation_deg);
    ae * ((r * theta.cos()) / (ae + h)).asin()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beam_height_matches_four_thirds_earth_reference() {
        // At 0° elevation, h ≈ r²/(2·aₑ): 100 km -> ~588 m.
        let h0 = beam_height_above_radar_m(100_000.0, 0.0);
        assert!((h0 - 588.6).abs() < 3.0, "0° 100km height was {h0}");

        // At 0.5° elevation, add ~r·sin(0.5°) ≈ 873 m -> ~1461 m.
        let h05 = beam_height_above_radar_m(100_000.0, 0.5);
        assert!((h05 - 1461.0).abs() < 5.0, "0.5° 100km height was {h05}");

        // Origin and monotonicity in range.
        assert!(beam_height_above_radar_m(0.0, 0.5).abs() < 1.0);
        assert!(
            beam_height_above_radar_m(200_000.0, 0.5) > beam_height_above_radar_m(100_000.0, 0.5)
        );
    }

    #[test]
    fn ground_range_close_to_slant_range_at_low_tilt() {
        // At low elevation the ground range is only slightly less than slant range.
        let s = beam_ground_range_m(100_000.0, 0.5);
        assert!(s > 99_000.0 && s < 100_000.0, "ground range was {s}");
    }

    #[test]
    fn moment_grid_scales_compact_u8_rows() {
        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            GateRange {
                first_gate_m: 0,
                gate_spacing_m: 250,
                gate_count: 3,
            },
            2.0,
            66.0,
            Some(0),
            Some(1),
        );

        grid.push_row(0, MomentRow::U8(vec![0, 66, 80])).unwrap();

        assert_eq!(grid.radial_count(), 1);
        assert_eq!(grid.scaled_value(0, 0), None);
        assert_eq!(grid.scaled_value(0, 1), Some(0.0));
        assert_eq!(grid.scaled_value(0, 2), Some(7.0));
    }

    #[test]
    fn moment_grid_expands_and_pads_variable_gate_rows() {
        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            GateRange {
                first_gate_m: 0,
                gate_spacing_m: 250,
                gate_count: 2,
            },
            2.0,
            66.0,
            Some(0),
            Some(1),
        );

        grid.push_row(0, MomentRow::U8(vec![66, 80])).unwrap();
        grid.push_row(1, MomentRow::U8(vec![66, 80, 90])).unwrap();

        assert_eq!(grid.gate_range.gate_count, 3);
        assert_eq!(grid.scaled_value(0, 2), None);
        assert_eq!(grid.scaled_value(1, 2), Some(12.0));
    }

    #[test]
    fn moment_grid_pushes_u8_slice_without_row_allocation() {
        let mut grid = MomentGrid::new_u8(
            MomentType::Velocity,
            GateRange {
                first_gate_m: 0,
                gate_spacing_m: 250,
                gate_count: 4,
            },
            2.0,
            129.0,
            Some(0),
            Some(1),
        );

        grid.push_u8_row_slice(2, &[129, 139]).unwrap();

        assert_eq!(grid.radial_indices, vec![2]);
        assert_eq!(grid.radial_count(), 1);
        assert_eq!(grid.scaled_value(0, 0), Some(0.0));
        assert_eq!(grid.scaled_value(0, 1), Some(5.0));
        assert_eq!(grid.scaled_value(0, 2), None);
        assert_eq!(grid.scaled_value(0, 3), None);
    }

    #[test]
    fn moment_grid_pushes_u16_be_bytes_without_row_allocation() {
        let mut grid = MomentGrid::new_u16(
            MomentType::DifferentialPhase,
            GateRange {
                first_gate_m: 0,
                gate_spacing_m: 250,
                gate_count: 4,
            },
            2.0,
            64.0,
            Some(0),
            Some(1),
        );

        grid.push_u16_be_row_bytes(2, &[0, 80, 0, 100, 0, 120])
            .unwrap();

        let MomentStorage::U16(values) = &grid.storage else {
            panic!("expected u16 storage");
        };
        assert_eq!(grid.radial_indices, vec![2]);
        assert_eq!(values, &vec![80, 100, 120, 0]);
        assert_eq!(grid.scaled_value(0, 0), Some(8.0));
        assert_eq!(grid.scaled_value(0, 3), None);
    }

    #[test]
    fn moment_grid_reserves_rows_and_gate_storage() {
        let mut grid = MomentGrid::new_u8(
            MomentType::Reflectivity,
            GateRange {
                first_gate_m: 0,
                gate_spacing_m: 250,
                gate_count: 3,
            },
            2.0,
            66.0,
            Some(0),
            Some(1),
        );

        grid.reserve_rows(4);

        assert!(grid.radial_indices.capacity() >= 4);
        let MomentStorage::U8(values) = &grid.storage else {
            panic!("expected u8 storage");
        };
        assert!(values.capacity() >= 12);
    }

    #[test]
    fn cut_tracks_available_moments() {
        let mut cut = ElevationCut::new(0.5, Some(1));
        cut.moments.insert(
            MomentType::Velocity,
            MomentGrid::new_u8(
                MomentType::Velocity,
                GateRange {
                    first_gate_m: 0,
                    gate_spacing_m: 250,
                    gate_count: 1,
                },
                2.0,
                129.0,
                Some(0),
                Some(1),
            ),
        );

        assert!(cut.moments_available().contains(&MomentType::Velocity));
    }

    #[test]
    fn moment_type_parses_padded_nexrad_bytes() {
        assert_eq!(
            MomentType::from_nexrad_bytes(b"SW "),
            MomentType::SpectrumWidth
        );
        assert_eq!(
            MomentType::from_nexrad_bytes(b"\0VEL"),
            MomentType::Velocity
        );
    }

    fn merge_gate_range(gate_count: usize) -> GateRange {
        GateRange {
            first_gate_m: 50,
            gate_spacing_m: 250,
            gate_count,
        }
    }

    fn merge_radial(azimuth_deg: f32, elevation_deg: f32, gate_count: usize) -> Radial {
        Radial {
            azimuth_deg,
            elevation_deg,
            time_offset_ms: 0,
            gate_range: merge_gate_range(gate_count),
            nyquist_velocity_mps: None,
            radial_status: None,
        }
    }

    /// A 3-radial cut carrying one u8 moment with `scale` recorded so tests
    /// can tell which part a surviving grid came from.
    fn merge_cut(elevation_deg: f32, moment: MomentType, scale: f32) -> ElevationCut {
        let mut cut = ElevationCut::new(elevation_deg, None);
        for index in 0..3 {
            cut.radials
                .push(merge_radial(index as f32 * 120.0, elevation_deg, 4));
        }
        let mut grid = MomentGrid::new_u8(
            moment.clone(),
            merge_gate_range(4),
            scale,
            66.0,
            Some(0),
            Some(1),
        );
        for index in 0..3 {
            grid.push_row(index, MomentRow::U8(vec![2, 3, 4, 5]))
                .unwrap();
        }
        cut.moments.insert(moment, grid);
        cut
    }

    fn merge_volume(site: &str, time_s: i64, cuts: Vec<ElevationCut>) -> RadarVolume {
        let mut volume = RadarVolume::new(
            RadarSite::new(site),
            DateTime::<Utc>::from_timestamp(time_s, 0).unwrap(),
        );
        volume.cuts = cuts;
        volume
    }

    #[test]
    fn merge_rejects_empty_input() {
        let err = merge_radar_volumes(Vec::new()).unwrap_err();
        assert!(err.contains("no radar volumes"), "unexpected error: {err}");
    }

    #[test]
    fn merge_single_part_is_identity_with_sorted_cuts() {
        let part = merge_volume(
            "SKJAV",
            1_000,
            vec![
                merge_cut(1.5, MomentType::Reflectivity, 2.0),
                merge_cut(0.5, MomentType::Reflectivity, 2.0),
            ],
        );
        let (merged, report) = merge_radar_volumes(vec![part.clone()]).unwrap();

        assert_eq!(report, MergeReport::default());
        assert_eq!(merged.site, part.site);
        assert_eq!(merged.volume_time, part.volume_time);
        // Identity up to the documented elevation sort.
        assert_eq!(merged.cuts.len(), 2);
        assert_eq!(merged.cuts[0].elevation_deg, 0.5);
        assert_eq!(merged.cuts[1].elevation_deg, 1.5);
    }

    #[test]
    fn merge_rejects_mismatched_site_ids() {
        let a = merge_volume("SKJAV", 0, vec![]);
        let b = merge_volume("SKKOJ", 0, vec![]);
        let err = merge_radar_volumes(vec![a, b]).unwrap_err();
        assert!(
            err.contains("SKJAV") && err.contains("SKKOJ"),
            "error must name both sites: {err}"
        );
    }

    #[test]
    fn merge_keeps_earliest_volume_time() {
        let a = merge_volume("SKJAV", 2_000, vec![]);
        let b = merge_volume("SKJAV", 1_000, vec![]);
        let c = merge_volume("SKJAV", 3_000, vec![]);
        let (merged, _) = merge_radar_volumes(vec![a, b, c]).unwrap();
        assert_eq!(
            merged.volume_time,
            DateTime::<Utc>::from_timestamp(1_000, 0).unwrap()
        );
    }

    #[test]
    fn merge_unions_moments_of_elevation_matched_cuts() {
        // SHMU-style split: one PVOL per product, same scan geometry, cut
        // elevations differing within the 0.05 deg tolerance.
        let dbz = merge_volume(
            "SKJAV",
            1_000,
            vec![merge_cut(0.5, MomentType::Reflectivity, 2.0)],
        );
        let vel = merge_volume(
            "SKJAV",
            1_010,
            vec![merge_cut(0.52, MomentType::Velocity, 2.0)],
        );
        let (merged, report) = merge_radar_volumes(vec![dbz, vel]).unwrap();

        assert_eq!(merged.cuts.len(), 1);
        let cut = &merged.cuts[0];
        assert_eq!(cut.elevation_deg, 0.5, "first part's cut is the base");
        assert!(cut.moments.contains_key(&MomentType::Reflectivity));
        assert!(cut.moments.contains_key(&MomentType::Velocity));
        assert_eq!(
            report,
            MergeReport {
                merged_moments: 1,
                skipped_geometry: 0,
                moment_collisions: 0,
            }
        );
    }

    #[test]
    fn merge_collision_keeps_first_part_grid() {
        let first = merge_volume(
            "SKJAV",
            1_000,
            vec![merge_cut(0.5, MomentType::Reflectivity, 2.0)],
        );
        let second = merge_volume(
            "SKJAV",
            1_000,
            vec![merge_cut(0.5, MomentType::Reflectivity, 4.0)],
        );
        let (merged, report) = merge_radar_volumes(vec![first, second]).unwrap();

        let grid = &merged.cuts[0].moments[&MomentType::Reflectivity];
        assert_eq!(grid.scale, 2.0, "first part must win the collision");
        assert_eq!(
            report,
            MergeReport {
                merged_moments: 0,
                skipped_geometry: 0,
                moment_collisions: 1,
            }
        );
    }

    #[test]
    fn merge_unions_unmatched_cuts_sorted_by_elevation() {
        let a = merge_volume(
            "SKJAV",
            1_000,
            vec![
                merge_cut(0.5, MomentType::Reflectivity, 2.0),
                merge_cut(2.5, MomentType::Reflectivity, 2.0),
            ],
        );
        let b = merge_volume(
            "SKJAV",
            1_000,
            vec![
                merge_cut(1.5, MomentType::Velocity, 2.0),
                merge_cut(0.2, MomentType::Velocity, 2.0),
            ],
        );
        let (merged, report) = merge_radar_volumes(vec![a, b]).unwrap();

        let elevations: Vec<f32> = merged.cuts.iter().map(|cut| cut.elevation_deg).collect();
        assert_eq!(elevations, vec![0.2, 0.5, 1.5, 2.5]);
        assert_eq!(report.merged_moments, 0);
        assert_eq!(report.skipped_geometry, 0);
    }

    #[test]
    fn merge_skips_matched_cut_with_different_radial_count() {
        let a = merge_volume(
            "SKJAV",
            1_000,
            vec![merge_cut(0.5, MomentType::Reflectivity, 2.0)],
        );
        let mut short_cut = merge_cut(0.5, MomentType::Velocity, 2.0);
        short_cut.radials.pop();
        let b = merge_volume("SKJAV", 1_000, vec![short_cut]);
        let (merged, report) = merge_radar_volumes(vec![a, b]).unwrap();

        assert_eq!(merged.cuts.len(), 1);
        assert!(!merged.cuts[0].moments.contains_key(&MomentType::Velocity));
        assert_eq!(report.skipped_geometry, 1);
        assert_eq!(report.merged_moments, 0);
    }

    #[test]
    fn merge_skips_matched_cut_with_shifted_azimuths() {
        let a = merge_volume(
            "SKJAV",
            1_000,
            vec![merge_cut(0.5, MomentType::Reflectivity, 2.0)],
        );
        let mut rotated = merge_cut(0.5, MomentType::Velocity, 2.0);
        for radial in &mut rotated.radials {
            radial.azimuth_deg += 1.0;
        }
        let b = merge_volume("SKJAV", 1_000, vec![rotated]);
        let (merged, report) = merge_radar_volumes(vec![a, b]).unwrap();

        assert!(!merged.cuts[0].moments.contains_key(&MomentType::Velocity));
        assert_eq!(report.skipped_geometry, 1);
    }

    #[test]
    fn merge_accepts_azimuths_equal_across_the_north_wrap() {
        let mut a_cut = merge_cut(0.5, MomentType::Reflectivity, 2.0);
        a_cut.radials[0].azimuth_deg = 359.99;
        let mut b_cut = merge_cut(0.5, MomentType::Velocity, 2.0);
        b_cut.radials[0].azimuth_deg = 0.01;
        let a = merge_volume("SKJAV", 1_000, vec![a_cut]);
        let b = merge_volume("SKJAV", 1_000, vec![b_cut]);
        let (merged, report) = merge_radar_volumes(vec![a, b]).unwrap();

        assert!(merged.cuts[0].moments.contains_key(&MomentType::Velocity));
        assert_eq!(report.merged_moments, 1);
        assert_eq!(report.skipped_geometry, 0);
    }

    #[test]
    fn merge_accepts_matched_cut_with_different_gate_layout() {
        let a = merge_volume(
            "SKJAV",
            1_000,
            vec![merge_cut(0.5, MomentType::Reflectivity, 2.0)],
        );
        let mut stretched = merge_cut(0.5, MomentType::Velocity, 2.0);
        for radial in &mut stretched.radials {
            radial.gate_range.gate_spacing_m = 1_000;
            radial.nyquist_velocity_mps = Some(18.5);
        }
        for grid in stretched.moments.values_mut() {
            grid.gate_range.gate_spacing_m = 1_000;
        }
        let b = merge_volume("SKJAV", 1_000, vec![stretched]);
        let (merged, report) = merge_radar_volumes(vec![a, b]).unwrap();

        let cut = &merged.cuts[0];
        assert!(cut.moments.contains_key(&MomentType::Velocity));
        assert_eq!(
            cut.moments[&MomentType::Velocity].gate_range.gate_spacing_m,
            1_000,
            "moment grid keeps its own range; radial range is only azimuth metadata"
        );
        assert_eq!(cut.radials[0].nyquist_velocity_mps, Some(18.5));
        assert_eq!(report.skipped_geometry, 0);
        assert_eq!(report.merged_moments, 1);
    }

    #[test]
    fn merge_three_product_parts_assembles_full_dual_pol_cut() {
        // DWD/CHMI-style assembly: same sweep from sweep_vol_z / _v / _zdr.
        let parts = vec![
            merge_volume(
                "DEASB",
                1_020,
                vec![merge_cut(0.5, MomentType::Reflectivity, 2.0)],
            ),
            merge_volume(
                "DEASB",
                1_000,
                vec![merge_cut(0.5, MomentType::Velocity, 2.0)],
            ),
            merge_volume(
                "DEASB",
                1_040,
                vec![merge_cut(0.5, MomentType::DifferentialReflectivity, 2.0)],
            ),
        ];
        let (merged, report) = merge_radar_volumes(parts).unwrap();

        assert_eq!(merged.cuts.len(), 1);
        assert_eq!(merged.cuts[0].moments.len(), 3);
        assert_eq!(
            merged.volume_time,
            DateTime::<Utc>::from_timestamp(1_000, 0).unwrap()
        );
        assert_eq!(report.merged_moments, 2);
        assert_eq!(report.moment_collisions, 0);
        assert_eq!(report.skipped_geometry, 0);
    }

    #[test]
    fn volume_can_keep_repeated_elevation_cuts_separate() {
        let mut volume = RadarVolume::new(RadarSite::new("TST"), Utc::now());

        volume.push_cut(0.5, Some(1));
        volume.push_cut(0.5, Some(1));

        assert_eq!(volume.cuts.len(), 2);
        let latest = volume.find_or_insert_cut(0.5, Some(1));
        latest.elevation_deg = 0.55;

        assert_eq!(volume.cuts[0].elevation_deg, 0.5);
        assert_eq!(volume.cuts[1].elevation_deg, 0.55);
    }
}
