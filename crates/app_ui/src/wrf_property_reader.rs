//! Sparse WRF P3/ISHMAEL property-field ingestion.
//!
//! This module is the file/normalization boundary in front of
//! `radar_scattering`'s grid-point closures.  It deliberately does not modify
//! the simulated-radar renderer or evaluate a LUT. A scene retains sparse
//! positive-mass category tuples plus dense `f32` temperature/air density so
//! clear corners and echo birth retain complete interpolation coverage. Full
//! WRF source fields are read one at a time and discarded.
//!
//! [`blend_raw_property_cells`] performs weighted spatial/temporal blending
//! before [`close_raw_property_cell`]. Closed particles, LUT coordinates, and
//! radar outputs are never blended. Scheme rain is read separately from the
//! Registry `QRAIN`/`QNRAIN` tuple; missing/invalid rain disables diagnostic
//! coexistence with a typed reason rather than an assumed PSD.
//!
//! WRF `mp_physics=55` has three prognostic ISHMAEL tuples: unsuffixed fields
//! are planar-nucleated ice, suffix `2` is columnar-nucleated ice, and suffix
//! `3` is aggregate ice.  `IshmaelIceCategory::{SmallIce,Rimed}` describe
//! physical closure states; they are not independent WRF field tuples and are
//! therefore never invented here.

use std::collections::BTreeSet;
use std::fmt;
use std::mem::size_of;
use std::sync::Arc;

use radar_scattering::{
    ClosedParticleCategory, ClosureContext, ClosureError, ConventionalCategoryInput,
    ConventionalHydrometeor, DiagnosticCoexistenceInput, DiagnosticCoexistenceResult,
    DiagnosticWetCategory, IshmaelCategoryInput, IshmaelDiagnostics, IshmaelIceCategory,
    IshmaelSourceFields, MixtureTopology, OrientationDefinition, P3Category, P3CategoryInput,
    ParticleEnvironment, close_conventional_category, close_ishmael_category, close_p3_category,
};
use thiserror::Error;
use wrf_core::WrfFile;

use crate::wrf_scene_inventory::WrfSourceIdentity;
use crate::wrf_temporal::ScenePropertySignature;

const WRF_REFERENCE_PRESSURE_PA: f64 = 100_000.0;
const WRF_KAPPA: f64 = 0.285_714_285_7;
const DRY_AIR_GAS_CONSTANT_J_KG_K: f64 = 287.05;
const WRF_FILL_MAGNITUDE: f64 = 1.0e30;

/// One raw field returned by a [`PropertyFieldProvider`].
#[derive(Clone, Debug, PartialEq)]
pub struct RawPropertyField {
    pub values: Vec<f64>,
    pub units: String,
}

impl RawPropertyField {
    #[must_use]
    pub fn new(values: Vec<f64>, units: impl Into<String>) -> Self {
        Self {
            values,
            units: units.into(),
        }
    }
}

/// Small provider seam used by the real [`WrfFile`] adapter and pure tests.
///
/// Implementations return one owned field at a time.  The reader calls
/// [`Self::clear_cache`] after every attempted read and again when its scope
/// ends, including on errors.
pub trait PropertyFieldProvider {
    fn source_identity(&self) -> WrfSourceIdentity;
    fn microphysics_scheme_id(&self) -> Result<i32, String>;
    fn cell_count(&self) -> usize;
    fn time_count(&self) -> usize;
    fn has_field(&self, name: &str) -> bool;
    fn read_field(&self, name: &str, time_index: usize) -> Result<RawPropertyField, String>;
    fn clear_cache(&self);
}

/// Real WRF provider.  The output scene stores only the supplied opaque
/// content identity; `WrfFile::path` is never copied into science identity or
/// provenance.
pub struct WrfFilePropertyProvider<'a> {
    file: &'a WrfFile,
    source_identity: WrfSourceIdentity,
}

impl<'a> WrfFilePropertyProvider<'a> {
    #[must_use]
    pub const fn new(file: &'a WrfFile, source_identity: WrfSourceIdentity) -> Self {
        Self {
            file,
            source_identity,
        }
    }
}

impl PropertyFieldProvider for WrfFilePropertyProvider<'_> {
    fn source_identity(&self) -> WrfSourceIdentity {
        self.source_identity.clone()
    }

    fn microphysics_scheme_id(&self) -> Result<i32, String> {
        self.file
            .global_attr_i32("MP_PHYSICS")
            .map_err(|error| error.to_string())
    }

    fn cell_count(&self) -> usize {
        self.file.nxyz()
    }

    fn time_count(&self) -> usize {
        self.file.nt
    }

    fn has_field(&self, name: &str) -> bool {
        self.file.has_var(name)
    }

    fn read_field(&self, name: &str, time_index: usize) -> Result<RawPropertyField, String> {
        let units = wrf_registry_units(name).ok_or_else(|| {
            format!("no WRF Registry unit contract is defined for property field {name}")
        })?;
        self.file
            .read_var(name, time_index)
            .map(|values| RawPropertyField::new(values, units))
            .map_err(|error| error.to_string())
    }

    fn clear_cache(&self) {
        self.file.clear_cache();
    }
}

/// Read a property scene from an already-open WRF file.
pub fn read_wrf_property_scene(
    file: &WrfFile,
    source_identity: WrfSourceIdentity,
    time_index: usize,
) -> Result<WrfPropertyScene, WrfPropertyReadError> {
    read_property_scene(
        &WrfFilePropertyProvider::new(file, source_identity),
        time_index,
    )
}

/// Canonical unit retained by compact fields.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum NormalizedUnit {
    KilogramsPerKilogram,
    PerKilogram,
    CubicMetersPerKilogram,
    Meters,
    KilogramsPerCubicMeter,
    MetersPerSecond,
    Kelvin,
    Pascal,
    Dimensionless,
}

impl NormalizedUnit {
    #[must_use]
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::KilogramsPerKilogram => "kg kg-1 dry air",
            Self::PerKilogram => "kg-1 dry air",
            Self::CubicMetersPerKilogram => "m3 kg-1 dry air",
            Self::Meters => "m",
            Self::KilogramsPerCubicMeter => "kg m-3",
            Self::MetersPerSecond => "m s-1",
            Self::Kelvin => "K",
            Self::Pascal => "Pa",
            Self::Dimensionless => "1",
        }
    }
}

/// Meaning assigned to a source field before a closure is evaluated.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SourceProperty {
    TotalIceMass,
    TotalIceNumber,
    RimeMass,
    RimeVolume,
    SixthMomentTransform,
    IceVolume,
    AspectWeightedIceVolume,
    DiagnosticDiameter,
    DiagnosticDensity,
    DiagnosticAspectRatio,
    DiagnosticFallSpeed,
    PerturbationPotentialTemperature,
    PerturbationPressure,
    BasePressure,
    WaterVaporMixingRatio,
    RainMass,
    RainNumber,
}

/// Unit conversion and semantic provenance for one field actually read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceFieldProvenance {
    source_name: &'static str,
    source_units: String,
    normalized_unit: NormalizedUnit,
    property: SourceProperty,
}

impl SourceFieldProvenance {
    #[must_use]
    pub const fn source_name(&self) -> &'static str {
        self.source_name
    }

    #[must_use]
    pub fn source_units(&self) -> &str {
        &self.source_units
    }

    #[must_use]
    pub const fn normalized_unit(&self) -> NormalizedUnit {
        self.normalized_unit
    }

    #[must_use]
    pub const fn property(&self) -> SourceProperty {
        self.property
    }
}

/// Field contract compared across adjacent property scenes.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RequiredFieldContract {
    pub source_name: &'static str,
    pub normalized_unit: NormalizedUnit,
    pub property: SourceProperty,
}

/// Exact source inventory that must agree before temporal raw-state blending.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequiredFieldSignature {
    pub microphysics_scheme_id: i32,
    pub fields: BTreeSet<RequiredFieldContract>,
}

impl RequiredFieldSignature {
    #[must_use]
    pub fn field_names(&self) -> BTreeSet<String> {
        self.fields
            .iter()
            .map(|field| field.source_name.to_owned())
            .collect()
    }
}

/// Exact WRF tuple identity.  ISHMAEL exposes only its three real tuples.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WrfPropertyCategory {
    P3(P3Category),
    IshmaelPlanar,
    IshmaelColumnar,
    IshmaelAggregate,
}

impl WrfPropertyCategory {
    #[must_use]
    pub const fn ishmael_category(self) -> Option<IshmaelIceCategory> {
        match self {
            Self::IshmaelPlanar => Some(IshmaelIceCategory::Planar),
            Self::IshmaelColumnar => Some(IshmaelIceCategory::Columnar),
            Self::IshmaelAggregate => Some(IshmaelIceCategory::Aggregate),
            Self::P3(_) => None,
        }
    }
}

impl fmt::Display for WrfPropertyCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::P3(P3Category::Category1) => formatter.write_str("P3 category 1"),
            Self::P3(P3Category::Category2) => formatter.write_str("P3 category 2"),
            Self::IshmaelPlanar => formatter.write_str("ISHMAEL planar tuple"),
            Self::IshmaelColumnar => formatter.write_str("ISHMAEL columnar tuple"),
            Self::IshmaelAggregate => formatter.write_str("ISHMAEL aggregate tuple"),
        }
    }
}

#[derive(Clone, Copy)]
struct FieldSpec {
    name: &'static str,
    unit: NormalizedUnit,
    property: SourceProperty,
}

impl FieldSpec {
    const fn new(name: &'static str, unit: NormalizedUnit, property: SourceProperty) -> Self {
        Self {
            name,
            unit,
            property,
        }
    }

    fn contract(self) -> RequiredFieldContract {
        RequiredFieldContract {
            source_name: self.name,
            normalized_unit: self.unit,
            property: self.property,
        }
    }
}

const T_FIELD: FieldSpec = FieldSpec::new(
    "T",
    NormalizedUnit::Kelvin,
    SourceProperty::PerturbationPotentialTemperature,
);
const P_FIELD: FieldSpec = FieldSpec::new(
    "P",
    NormalizedUnit::Pascal,
    SourceProperty::PerturbationPressure,
);
const PB_FIELD: FieldSpec =
    FieldSpec::new("PB", NormalizedUnit::Pascal, SourceProperty::BasePressure);
const QVAPOR_FIELD: FieldSpec = FieldSpec::new(
    "QVAPOR",
    NormalizedUnit::KilogramsPerKilogram,
    SourceProperty::WaterVaporMixingRatio,
);
const QRAIN_FIELD: FieldSpec = FieldSpec::new(
    "QRAIN",
    NormalizedUnit::KilogramsPerKilogram,
    SourceProperty::RainMass,
);
const QNRAIN_FIELD: FieldSpec = FieldSpec::new(
    "QNRAIN",
    NormalizedUnit::PerKilogram,
    SourceProperty::RainNumber,
);

#[derive(Clone, Copy)]
struct P3Spec {
    category: P3Category,
    qice: FieldSpec,
    qnice: FieldSpec,
    qir: FieldSpec,
    qib: FieldSpec,
    qzi: Option<FieldSpec>,
}

const P3_CATEGORY_1: P3Spec = P3Spec {
    category: P3Category::Category1,
    qice: FieldSpec::new(
        "QICE",
        NormalizedUnit::KilogramsPerKilogram,
        SourceProperty::TotalIceMass,
    ),
    qnice: FieldSpec::new(
        "QNICE",
        NormalizedUnit::PerKilogram,
        SourceProperty::TotalIceNumber,
    ),
    qir: FieldSpec::new(
        "QIR",
        NormalizedUnit::KilogramsPerKilogram,
        SourceProperty::RimeMass,
    ),
    qib: FieldSpec::new(
        "QIB",
        NormalizedUnit::CubicMetersPerKilogram,
        SourceProperty::RimeVolume,
    ),
    qzi: None,
};

const P3_CATEGORY_2: P3Spec = P3Spec {
    category: P3Category::Category2,
    qice: FieldSpec::new(
        "QICE2",
        NormalizedUnit::KilogramsPerKilogram,
        SourceProperty::TotalIceMass,
    ),
    qnice: FieldSpec::new(
        "QNICE2",
        NormalizedUnit::PerKilogram,
        SourceProperty::TotalIceNumber,
    ),
    qir: FieldSpec::new(
        "QIR2",
        NormalizedUnit::KilogramsPerKilogram,
        SourceProperty::RimeMass,
    ),
    qib: FieldSpec::new(
        "QIB2",
        NormalizedUnit::CubicMetersPerKilogram,
        SourceProperty::RimeVolume,
    ),
    qzi: None,
};

const QZI_FIELD: FieldSpec = FieldSpec::new(
    "QZI",
    NormalizedUnit::CubicMetersPerKilogram,
    SourceProperty::SixthMomentTransform,
);

#[derive(Clone, Copy)]
struct IshmaelSpec {
    category: WrfPropertyCategory,
    qice: FieldSpec,
    qnice: FieldSpec,
    qvoli: FieldSpec,
    qaoli: FieldSpec,
    d_ice: FieldSpec,
    rho_ice: FieldSpec,
    phi_ice: FieldSpec,
    v_ice: FieldSpec,
}

#[allow(clippy::too_many_arguments)]
const fn ishmael_spec(
    category: WrfPropertyCategory,
    qice: &'static str,
    qnice: &'static str,
    qvoli: &'static str,
    qaoli: &'static str,
    d_ice: &'static str,
    rho_ice: &'static str,
    phi_ice: &'static str,
    v_ice: &'static str,
) -> IshmaelSpec {
    IshmaelSpec {
        category,
        qice: FieldSpec::new(
            qice,
            NormalizedUnit::KilogramsPerKilogram,
            SourceProperty::TotalIceMass,
        ),
        qnice: FieldSpec::new(
            qnice,
            NormalizedUnit::PerKilogram,
            SourceProperty::TotalIceNumber,
        ),
        qvoli: FieldSpec::new(
            qvoli,
            NormalizedUnit::CubicMetersPerKilogram,
            SourceProperty::IceVolume,
        ),
        qaoli: FieldSpec::new(
            qaoli,
            NormalizedUnit::CubicMetersPerKilogram,
            SourceProperty::AspectWeightedIceVolume,
        ),
        d_ice: FieldSpec::new(
            d_ice,
            NormalizedUnit::Meters,
            SourceProperty::DiagnosticDiameter,
        ),
        rho_ice: FieldSpec::new(
            rho_ice,
            NormalizedUnit::KilogramsPerCubicMeter,
            SourceProperty::DiagnosticDensity,
        ),
        phi_ice: FieldSpec::new(
            phi_ice,
            NormalizedUnit::Dimensionless,
            SourceProperty::DiagnosticAspectRatio,
        ),
        v_ice: FieldSpec::new(
            v_ice,
            NormalizedUnit::MetersPerSecond,
            SourceProperty::DiagnosticFallSpeed,
        ),
    }
}

const ISHMAEL_SPECS: [IshmaelSpec; 3] = [
    ishmael_spec(
        WrfPropertyCategory::IshmaelPlanar,
        "QICE",
        "QNICE",
        "QVOLI",
        "QAOLI",
        "D_ICE",
        "RHO_ICE",
        "PHI_ICE",
        "V_ICE",
    ),
    ishmael_spec(
        WrfPropertyCategory::IshmaelColumnar,
        "QICE2",
        "QNICE2",
        "QVOLI2",
        "QAOLI2",
        "D_ICE2",
        "RHO_ICE2",
        "PHI_ICE2",
        "V_ICE2",
    ),
    ishmael_spec(
        WrfPropertyCategory::IshmaelAggregate,
        "QICE3",
        "QNICE3",
        "QVOLI3",
        "QAOLI3",
        "D_ICE3",
        "RHO_ICE3",
        "PHI_ICE3",
        "V_ICE3",
    ),
];

#[derive(Clone, Debug, PartialEq)]
struct SparseDiagnostic {
    cell_indices: Vec<u32>,
    values: Vec<f32>,
}

impl SparseDiagnostic {
    fn value_at(&self, cell_index: u32) -> Option<f64> {
        self.cell_indices
            .binary_search(&cell_index)
            .ok()
            .map(|position| f64::from(self.values[position]))
    }

    fn index_bytes(&self) -> usize {
        self.cell_indices.len() * size_of::<u32>()
    }

    fn value_bytes(&self) -> usize {
        self.values.len() * size_of::<f32>()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct IshmaelDiagnosticStorage {
    d_ice: Option<SparseDiagnostic>,
    rho_ice: Option<SparseDiagnostic>,
    phi_ice: Option<SparseDiagnostic>,
    v_ice: Option<SparseDiagnostic>,
}

impl IshmaelDiagnosticStorage {
    fn index_bytes(&self) -> usize {
        [
            self.d_ice.as_ref(),
            self.rho_ice.as_ref(),
            self.phi_ice.as_ref(),
            self.v_ice.as_ref(),
        ]
        .into_iter()
        .flatten()
        .map(SparseDiagnostic::index_bytes)
        .sum()
    }

    fn value_bytes(&self) -> usize {
        [
            self.d_ice.as_ref(),
            self.rho_ice.as_ref(),
            self.phi_ice.as_ref(),
            self.v_ice.as_ref(),
        ]
        .into_iter()
        .flatten()
        .map(SparseDiagnostic::value_bytes)
        .sum()
    }
}

#[derive(Clone, Debug, PartialEq)]
enum SparseCategoryValues {
    P3 {
        qice_kgkg: Vec<f32>,
        qnice_per_kg: Vec<f32>,
        qir_kgkg: Vec<f32>,
        qib_m3_per_kg: Vec<f32>,
        qzi: Option<Vec<f32>>,
    },
    Ishmael {
        qice_kgkg: Vec<f32>,
        qnice_per_kg: Vec<f32>,
        qvoli_m3_per_kg: Vec<f32>,
        qaoli_m3_per_kg: Vec<f32>,
        diagnostics: Box<IshmaelDiagnosticStorage>,
        source_names: IshmaelSourceFields,
    },
}

impl SparseCategoryValues {
    fn value_bytes(&self) -> usize {
        let f32_bytes = size_of::<f32>();
        match self {
            Self::P3 {
                qice_kgkg,
                qnice_per_kg,
                qir_kgkg,
                qib_m3_per_kg,
                qzi,
            } => {
                (qice_kgkg.len()
                    + qnice_per_kg.len()
                    + qir_kgkg.len()
                    + qib_m3_per_kg.len()
                    + qzi.as_ref().map_or(0, Vec::len))
                    * f32_bytes
            }
            Self::Ishmael {
                qice_kgkg,
                qnice_per_kg,
                qvoli_m3_per_kg,
                qaoli_m3_per_kg,
                diagnostics,
                source_names: _,
            } => {
                (qice_kgkg.len()
                    + qnice_per_kg.len()
                    + qvoli_m3_per_kg.len()
                    + qaoli_m3_per_kg.len())
                    * f32_bytes
                    + diagnostics.value_bytes()
            }
        }
    }

    fn diagnostic_index_bytes(&self) -> usize {
        match self {
            Self::P3 { .. } => 0,
            Self::Ishmael { diagnostics, .. } => diagnostics.index_bytes(),
        }
    }
}

/// One compact positive-mass WRF category.
#[derive(Clone, Debug, PartialEq)]
pub struct SparsePropertyCategory {
    category: WrfPropertyCategory,
    active_cell_indices: Vec<u32>,
    source_fields: Vec<&'static str>,
    values: SparseCategoryValues,
}

impl SparsePropertyCategory {
    #[must_use]
    pub const fn category(&self) -> WrfPropertyCategory {
        self.category
    }

    #[must_use]
    pub fn active_cell_indices(&self) -> &[u32] {
        &self.active_cell_indices
    }

    #[must_use]
    pub fn source_fields(&self) -> &[&'static str] {
        &self.source_fields
    }

    fn position(&self, cell_index: u32) -> Option<usize> {
        self.active_cell_indices.binary_search(&cell_index).ok()
    }
}

/// Why the file cannot provide a scientifically closable liquid-rain PSD.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RainUnavailableReason {
    MissingMassField {
        field: &'static str,
    },
    MissingNumberField {
        field: &'static str,
    },
    InvalidField {
        field: &'static str,
        message: String,
    },
}

impl fmt::Display for RainUnavailableReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingMassField { field } => {
                write!(formatter, "WRF rain mass field {field} is absent")
            }
            Self::MissingNumberField { field } => {
                write!(formatter, "WRF rain number field {field} is absent")
            }
            Self::InvalidField { field, message } => {
                write!(formatter, "WRF rain field {field} is unusable: {message}")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum SparseRainStorage {
    Available {
        active_cell_indices: Vec<u32>,
        qrain_kgkg: Vec<f32>,
        qnrain_per_kg: Vec<f32>,
    },
    Unavailable(RainUnavailableReason),
}

impl SparseRainStorage {
    fn raw_at(&self, cell_index: u32) -> RawRainState {
        match self {
            Self::Unavailable(reason) => RawRainState::Unavailable(reason.clone()),
            Self::Available {
                active_cell_indices,
                qrain_kgkg,
                qnrain_per_kg,
            } => match active_cell_indices.binary_search(&cell_index) {
                Ok(position) => RawRainState::Available {
                    qrain_kgkg: f64::from(qrain_kgkg[position]),
                    qnrain_per_kg: f64::from(qnrain_per_kg[position]),
                },
                Err(_) => RawRainState::Available {
                    qrain_kgkg: 0.0,
                    qnrain_per_kg: 0.0,
                },
            },
        }
    }

    fn index_bytes(&self) -> usize {
        match self {
            Self::Available {
                active_cell_indices,
                ..
            } => active_cell_indices.len() * size_of::<u32>(),
            Self::Unavailable(_) => 0,
        }
    }

    fn value_bytes(&self) -> usize {
        match self {
            Self::Available {
                qrain_kgkg,
                qnrain_per_kg,
                ..
            } => (qrain_kgkg.len() + qnrain_per_kg.len()) * size_of::<f32>(),
            Self::Unavailable(_) => 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct DenseEnvironment {
    temperature_k: Vec<f32>,
    air_density_kg_m3: Vec<f32>,
}

impl DenseEnvironment {
    fn environment_at(&self, cell_index: u32) -> Option<ParticleEnvironment> {
        ParticleEnvironment::new(
            f64::from(*self.temperature_k.get(cell_index as usize)?),
            f64::from(*self.air_density_kg_m3.get(cell_index as usize)?),
        )
        .ok()
    }
}

/// Identity retained by a property scene.  It deliberately has no path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PropertySceneIdentity {
    pub source_identity: WrfSourceIdentity,
    pub time_index: usize,
}

/// Unit-normalized P3 state before nonlinear closure or LUT lookup.
#[derive(Clone, Debug, PartialEq)]
pub struct RawP3Category {
    pub category: P3Category,
    pub qice_kgkg: f64,
    pub qnice_per_kg: f64,
    pub qir_kgkg: f64,
    pub qib_m3_per_kg: f64,
    pub qzi: Option<f64>,
}

/// Unit-normalized ISHMAEL state before nonlinear closure or LUT lookup.
#[derive(Clone, Debug, PartialEq)]
pub struct RawIshmaelCategory {
    pub category: WrfPropertyCategory,
    pub qice_kgkg: f64,
    pub qnice_per_kg: f64,
    pub qvoli_m3_per_kg: f64,
    pub qaoli_m3_per_kg: f64,
    pub diagnostics: IshmaelDiagnostics,
    pub source_names: IshmaelSourceFields,
}

/// One raw frozen tuple.  Zero mass is explicit clear state, never missing
/// spatial/temporal coverage.
#[derive(Clone, Debug, PartialEq)]
pub enum RawPropertyCategory {
    P3(RawP3Category),
    Ishmael(RawIshmaelCategory),
}

impl RawPropertyCategory {
    #[must_use]
    pub const fn category(&self) -> WrfPropertyCategory {
        match self {
            Self::P3(value) => WrfPropertyCategory::P3(value.category),
            Self::Ishmael(value) => value.category,
        }
    }

    #[must_use]
    pub const fn mixing_ratio_kgkg(&self) -> f64 {
        match self {
            Self::P3(value) => value.qice_kgkg,
            Self::Ishmael(value) => value.qice_kgkg,
        }
    }
}

/// Raw rain availability at one cell.  Available zero mass is clear rain;
/// `Unavailable` means the file cannot provide a valid PSD for coexistence.
#[derive(Clone, Debug, PartialEq)]
pub enum RawRainState {
    Available { qrain_kgkg: f64, qnrain_per_kg: f64 },
    Unavailable(RainUnavailableReason),
}

/// Complete raw model state at one cell or weighted gate.  It is safe to
/// blend; no particle closure or scattering quantity has been evaluated.
#[derive(Clone, Debug, PartialEq)]
pub struct RawPropertyCell {
    source_cell_index: Option<u32>,
    microphysics_scheme_id: i32,
    required_field_signature: Arc<RequiredFieldSignature>,
    environment: ParticleEnvironment,
    categories: Vec<RawPropertyCategory>,
    rain: RawRainState,
}

impl RawPropertyCell {
    #[must_use]
    pub const fn source_cell_index(&self) -> Option<u32> {
        self.source_cell_index
    }

    #[must_use]
    pub const fn microphysics_scheme_id(&self) -> i32 {
        self.microphysics_scheme_id
    }

    #[must_use]
    pub fn required_field_signature(&self) -> &RequiredFieldSignature {
        self.required_field_signature.as_ref()
    }

    #[must_use]
    pub const fn environment(&self) -> ParticleEnvironment {
        self.environment
    }

    #[must_use]
    pub fn categories(&self) -> &[RawPropertyCategory] {
        &self.categories
    }

    #[must_use]
    pub const fn rain(&self) -> &RawRainState {
        &self.rain
    }
}

/// One spatial or temporal contributor to a raw gate sample.
#[derive(Clone, Copy, Debug)]
pub struct WeightedRawPropertyCell<'a> {
    pub scene: &'a WrfPropertyScene,
    pub cell_index: usize,
    pub weight: f64,
}

impl<'a> WeightedRawPropertyCell<'a> {
    #[must_use]
    pub const fn new(scene: &'a WrfPropertyScene, cell_index: usize, weight: f64) -> Self {
        Self {
            scene,
            cell_index,
            weight,
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum RawPropertyBlendError {
    #[error("raw property interpolation requires at least one weighted cell")]
    NoSamples,
    #[error("raw property weight {weight} at sample {sample_index} must be finite and nonnegative")]
    InvalidWeight { sample_index: usize, weight: f64 },
    #[error("raw property weights sum to {sum}, expected 1")]
    WeightSum { sum: f64 },
    #[error("raw property sample {sample_index} has a different required-field signature")]
    FieldSignatureMismatch { sample_index: usize },
    #[error("raw property sample {sample_index} has a different category layout")]
    CategoryLayoutMismatch { sample_index: usize },
    #[error("raw rain availability differs at sample {sample_index}")]
    RainAvailabilityMismatch { sample_index: usize },
    #[error(transparent)]
    Sample(#[from] WrfPropertyReadError),
    #[error("blended raw environment is invalid: {source}")]
    Environment {
        #[source]
        source: radar_scattering::ParticleError,
    },
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum RawPropertyClosureError {
    #[error("close raw environment: {source}")]
    Environment {
        #[source]
        source: ClosureError,
    },
    #[error("close raw {category}: {source}")]
    Category {
        category: WrfPropertyCategory,
        #[source]
        source: ClosureError,
    },
    #[error("close raw rain: {source}")]
    Rain {
        #[source]
        source: ClosureError,
    },
}

/// Compact normalized inputs for one WRF model time.
#[derive(Clone, Debug, PartialEq)]
pub struct WrfPropertyScene {
    identity: PropertySceneIdentity,
    microphysics_scheme_id: i32,
    cell_count: usize,
    active_cell_indices: Vec<u32>,
    environment: DenseEnvironment,
    categories: Vec<SparsePropertyCategory>,
    rain: SparseRainStorage,
    skipped_zero_mass_categories: Vec<WrfPropertyCategory>,
    ishmael_source_names: Vec<(WrfPropertyCategory, IshmaelSourceFields)>,
    source_fields: Vec<SourceFieldProvenance>,
    required_field_signature: Arc<RequiredFieldSignature>,
}

impl WrfPropertyScene {
    #[must_use]
    pub const fn identity(&self) -> &PropertySceneIdentity {
        &self.identity
    }

    #[must_use]
    pub const fn microphysics_scheme_id(&self) -> i32 {
        self.microphysics_scheme_id
    }

    #[must_use]
    pub const fn cell_count(&self) -> usize {
        self.cell_count
    }

    #[must_use]
    pub fn active_cell_indices(&self) -> &[u32] {
        &self.active_cell_indices
    }

    #[must_use]
    pub fn categories(&self) -> &[SparsePropertyCategory] {
        &self.categories
    }

    #[must_use]
    pub fn skipped_zero_mass_categories(&self) -> &[WrfPropertyCategory] {
        &self.skipped_zero_mass_categories
    }

    #[must_use]
    pub fn source_fields(&self) -> &[SourceFieldProvenance] {
        &self.source_fields
    }

    #[must_use]
    pub fn required_field_signature(&self) -> &RequiredFieldSignature {
        self.required_field_signature.as_ref()
    }

    /// Bridge to the temporal module without claiming a renderer/LUT source.
    /// The eventual renderer supplies its actual reflectivity-source label.
    #[must_use]
    pub fn temporal_signature(
        &self,
        reflectivity_source: impl Into<String>,
    ) -> ScenePropertySignature {
        ScenePropertySignature {
            microphysics_scheme_id: Some(self.microphysics_scheme_id),
            reflectivity_source: reflectivity_source.into(),
            required_raw_fields: self.required_field_signature.field_names(),
        }
    }

    pub fn environment_at(
        &self,
        cell_index: usize,
    ) -> Result<Option<ParticleEnvironment>, WrfPropertyReadError> {
        let compact_index = self.checked_cell_index(cell_index)?;
        Ok(self.environment.environment_at(compact_index))
    }

    /// Return complete raw state for interpolation. Every scheme category is
    /// present; cells outside sparse positive-mass indices are explicit zero
    /// tuples rather than missing spatial coverage.
    pub fn raw_cell(&self, cell_index: usize) -> Result<RawPropertyCell, WrfPropertyReadError> {
        let compact_index = self.checked_cell_index(cell_index)?;
        let environment = self.environment.environment_at(compact_index).ok_or(
            WrfPropertyReadError::InvalidEnvironment {
                cell_index,
                reason: "dense temperature/air-density coverage is unavailable",
            },
        )?;
        let categories = if matches!(self.microphysics_scheme_id, 50..=53) {
            let expected = if self.microphysics_scheme_id == 52 {
                &[P3Category::Category1, P3Category::Category2][..]
            } else {
                &[P3Category::Category1][..]
            };
            expected
                .iter()
                .copied()
                .map(|p3_category| {
                    let identity = WrfPropertyCategory::P3(p3_category);
                    let stored = self
                        .categories
                        .iter()
                        .find(|category| category.category == identity)
                        .and_then(|category| {
                            category
                                .position(compact_index)
                                .map(|position| (category, position))
                        });
                    match stored {
                        Some((category, position)) => {
                            let SparseCategoryValues::P3 {
                                qice_kgkg,
                                qnice_per_kg,
                                qir_kgkg,
                                qib_m3_per_kg,
                                qzi,
                            } = &category.values
                            else {
                                unreachable!("P3 identity has P3 storage")
                            };
                            RawPropertyCategory::P3(RawP3Category {
                                category: p3_category,
                                qice_kgkg: f64::from(qice_kgkg[position]),
                                qnice_per_kg: f64::from(qnice_per_kg[position]),
                                qir_kgkg: f64::from(qir_kgkg[position]),
                                qib_m3_per_kg: f64::from(qib_m3_per_kg[position]),
                                qzi: qzi.as_ref().map(|values| f64::from(values[position])),
                            })
                        }
                        None => RawPropertyCategory::P3(RawP3Category {
                            category: p3_category,
                            qice_kgkg: 0.0,
                            qnice_per_kg: 0.0,
                            qir_kgkg: 0.0,
                            qib_m3_per_kg: 0.0,
                            qzi: (self.microphysics_scheme_id == 53).then_some(0.0),
                        }),
                    }
                })
                .collect()
        } else {
            [
                WrfPropertyCategory::IshmaelPlanar,
                WrfPropertyCategory::IshmaelColumnar,
                WrfPropertyCategory::IshmaelAggregate,
            ]
            .into_iter()
            .map(|identity| {
                let source_names = self
                    .ishmael_source_names
                    .iter()
                    .find_map(|(category, names)| (*category == identity).then_some(*names))
                    .expect("ISHMAEL scene retains all three source maps");
                let stored = self
                    .categories
                    .iter()
                    .find(|category| category.category == identity)
                    .and_then(|category| {
                        category
                            .position(compact_index)
                            .map(|position| (category, position))
                    });
                match stored {
                    Some((category, position)) => {
                        let SparseCategoryValues::Ishmael {
                            qice_kgkg,
                            qnice_per_kg,
                            qvoli_m3_per_kg,
                            qaoli_m3_per_kg,
                            diagnostics,
                            ..
                        } = &category.values
                        else {
                            unreachable!("ISHMAEL identity has ISHMAEL storage")
                        };
                        RawPropertyCategory::Ishmael(RawIshmaelCategory {
                            category: identity,
                            qice_kgkg: f64::from(qice_kgkg[position]),
                            qnice_per_kg: f64::from(qnice_per_kg[position]),
                            qvoli_m3_per_kg: f64::from(qvoli_m3_per_kg[position]),
                            qaoli_m3_per_kg: f64::from(qaoli_m3_per_kg[position]),
                            diagnostics: IshmaelDiagnostics::new(
                                diagnostics
                                    .d_ice
                                    .as_ref()
                                    .and_then(|field| field.value_at(compact_index)),
                                diagnostics
                                    .rho_ice
                                    .as_ref()
                                    .and_then(|field| field.value_at(compact_index)),
                                diagnostics
                                    .phi_ice
                                    .as_ref()
                                    .and_then(|field| field.value_at(compact_index)),
                                diagnostics
                                    .v_ice
                                    .as_ref()
                                    .and_then(|field| field.value_at(compact_index)),
                            ),
                            source_names,
                        })
                    }
                    None => RawPropertyCategory::Ishmael(RawIshmaelCategory {
                        category: identity,
                        qice_kgkg: 0.0,
                        qnice_per_kg: 0.0,
                        qvoli_m3_per_kg: 0.0,
                        qaoli_m3_per_kg: 0.0,
                        diagnostics: IshmaelDiagnostics::default(),
                        source_names,
                    }),
                }
            })
            .collect()
        };
        Ok(RawPropertyCell {
            source_cell_index: Some(compact_index),
            microphysics_scheme_id: self.microphysics_scheme_id,
            required_field_signature: Arc::clone(&self.required_field_signature),
            environment,
            categories,
            rain: self.rain.raw_at(compact_index),
        })
    }

    /// Close every positive-mass tuple at one cell into `radar_scattering`
    /// particle/property types.  A clear cell returns `Ok(None)`.
    pub fn close_cell(
        &self,
        cell_index: usize,
        orientation: OrientationDefinition,
    ) -> Result<Option<ClosedPropertyCell>, WrfPropertyReadError> {
        let raw = self.raw_cell(cell_index)?;
        let rain_mass = match raw.rain() {
            RawRainState::Available { qrain_kgkg, .. } => *qrain_kgkg,
            RawRainState::Unavailable(_) => 0.0,
        };
        if raw
            .categories()
            .iter()
            .all(|category| category.mixing_ratio_kgkg() == 0.0)
            && rain_mass == 0.0
        {
            return Ok(None);
        }
        close_raw_property_cell(&raw, orientation)
            .map(Some)
            .map_err(|error| match error {
                RawPropertyClosureError::Environment { source } => {
                    WrfPropertyReadError::EnvironmentClosure { cell_index, source }
                }
                RawPropertyClosureError::Category { category, source } => {
                    WrfPropertyReadError::CategoryClosure {
                        cell_index,
                        category,
                        source,
                    }
                }
                RawPropertyClosureError::Rain { source } => {
                    WrfPropertyReadError::RainClosure { cell_index, source }
                }
            })
    }

    #[must_use]
    pub fn memory_estimate(&self) -> PropertyMemoryEstimate {
        let category_index_bytes = self
            .categories
            .iter()
            .map(|category| category.active_cell_indices.len() * size_of::<u32>())
            .sum::<usize>();
        let diagnostic_index_bytes = self
            .categories
            .iter()
            .map(|category| category.values.diagnostic_index_bytes())
            .sum::<usize>();
        let index_bytes = self.active_cell_indices.len() * size_of::<u32>()
            + category_index_bytes
            + diagnostic_index_bytes
            + self.rain.index_bytes();
        let value_bytes = (self.environment.temperature_k.len()
            + self.environment.air_density_kg_m3.len())
            * size_of::<f32>()
            + self
                .categories
                .iter()
                .map(|category| category.values.value_bytes())
                .sum::<usize>()
            + self.rain.value_bytes();
        let structure_bytes = size_of::<Self>()
            + self.categories.len() * size_of::<SparsePropertyCategory>()
            + self.skipped_zero_mass_categories.len() * size_of::<WrfPropertyCategory>()
            + self.ishmael_source_names.len()
                * size_of::<(WrfPropertyCategory, IshmaelSourceFields)>()
            + self.source_fields.len() * size_of::<SourceFieldProvenance>()
            + self.required_field_signature.fields.len() * size_of::<RequiredFieldContract>()
            + self
                .categories
                .iter()
                .filter(|category| matches!(&category.values, SparseCategoryValues::Ishmael { .. }))
                .count()
                * size_of::<IshmaelDiagnosticStorage>()
            + self
                .categories
                .iter()
                .map(|category| category.source_fields.len() * size_of::<&'static str>())
                .sum::<usize>();
        let identity_bytes = self.identity.source_identity.0.len();
        let rain_reason_text_bytes = match &self.rain {
            SparseRainStorage::Unavailable(RainUnavailableReason::InvalidField {
                message, ..
            }) => message.len(),
            _ => 0,
        };
        let provenance_text_bytes = self
            .source_fields
            .iter()
            .map(|field| field.source_units.len())
            .sum::<usize>()
            + rain_reason_text_bytes;
        PropertyMemoryEstimate {
            structure_bytes,
            index_bytes,
            value_bytes,
            identity_bytes,
            provenance_text_bytes,
        }
    }

    fn checked_cell_index(&self, cell_index: usize) -> Result<u32, WrfPropertyReadError> {
        if cell_index >= self.cell_count {
            return Err(WrfPropertyReadError::CellOutOfRange {
                cell_index,
                cell_count: self.cell_count,
            });
        }
        u32::try_from(cell_index).map_err(|_| WrfPropertyReadError::GridTooLarge {
            cell_count: self.cell_count,
        })
    }
}

/// Blend normalized raw cells in space, time, or both.  Callers pass the
/// product weights for all contributing corners/scenes. Required-field
/// signatures and category layouts must match exactly. No closed particle or
/// radar quantity is ever interpolated here.
pub fn blend_raw_property_cells(
    samples: &[WeightedRawPropertyCell<'_>],
) -> Result<RawPropertyCell, RawPropertyBlendError> {
    let Some(first_sample) = samples.first() else {
        return Err(RawPropertyBlendError::NoSamples);
    };
    let mut weight_sum = 0.0;
    for (sample_index, sample) in samples.iter().enumerate() {
        if !sample.weight.is_finite() || sample.weight < 0.0 {
            return Err(RawPropertyBlendError::InvalidWeight {
                sample_index,
                weight: sample.weight,
            });
        }
        if sample.scene.required_field_signature() != first_sample.scene.required_field_signature()
        {
            return Err(RawPropertyBlendError::FieldSignatureMismatch { sample_index });
        }
        weight_sum += sample.weight;
    }
    if (weight_sum - 1.0).abs() > 1.0e-9 {
        return Err(RawPropertyBlendError::WeightSum { sum: weight_sum });
    }
    let raw_cells = samples
        .iter()
        .map(|sample| sample.scene.raw_cell(sample.cell_index))
        .collect::<Result<Vec<_>, _>>()?;
    let first = &raw_cells[0];
    for (sample_index, cell) in raw_cells.iter().enumerate() {
        if cell.categories.len() != first.categories.len()
            || cell
                .categories
                .iter()
                .zip(&first.categories)
                .any(|(left, right)| left.category() != right.category())
        {
            return Err(RawPropertyBlendError::CategoryLayoutMismatch { sample_index });
        }
    }

    let temperature_k = raw_cells
        .iter()
        .zip(samples)
        .map(|(cell, sample)| cell.environment.temperature_k() * sample.weight)
        .sum();
    let air_density_kg_m3 = raw_cells
        .iter()
        .zip(samples)
        .map(|(cell, sample)| cell.environment.air_density_kg_m3() * sample.weight)
        .sum();
    let environment = ParticleEnvironment::new(temperature_k, air_density_kg_m3)
        .map_err(|source| RawPropertyBlendError::Environment { source })?;

    let mut categories = Vec::with_capacity(first.categories.len());
    for category_index in 0..first.categories.len() {
        match &first.categories[category_index] {
            RawPropertyCategory::P3(first_value) => {
                let mut output = RawP3Category {
                    category: first_value.category,
                    qice_kgkg: 0.0,
                    qnice_per_kg: 0.0,
                    qir_kgkg: 0.0,
                    qib_m3_per_kg: 0.0,
                    qzi: first_value.qzi.map(|_| 0.0),
                };
                for (sample_index, (cell, sample)) in raw_cells.iter().zip(samples).enumerate() {
                    let RawPropertyCategory::P3(value) = &cell.categories[category_index] else {
                        return Err(RawPropertyBlendError::CategoryLayoutMismatch { sample_index });
                    };
                    if value.category != output.category
                        || value.qzi.is_some() != output.qzi.is_some()
                    {
                        return Err(RawPropertyBlendError::CategoryLayoutMismatch { sample_index });
                    }
                    output.qice_kgkg += sample.weight * value.qice_kgkg;
                    output.qnice_per_kg += sample.weight * value.qnice_per_kg;
                    output.qir_kgkg += sample.weight * value.qir_kgkg;
                    output.qib_m3_per_kg += sample.weight * value.qib_m3_per_kg;
                    if let (Some(output_qzi), Some(value_qzi)) = (&mut output.qzi, value.qzi) {
                        *output_qzi += sample.weight * value_qzi;
                    }
                }
                categories.push(RawPropertyCategory::P3(output));
            }
            RawPropertyCategory::Ishmael(first_value) => {
                let mut output = RawIshmaelCategory {
                    category: first_value.category,
                    qice_kgkg: 0.0,
                    qnice_per_kg: 0.0,
                    qvoli_m3_per_kg: 0.0,
                    qaoli_m3_per_kg: 0.0,
                    diagnostics: IshmaelDiagnostics::default(),
                    source_names: first_value.source_names,
                };
                for (sample_index, (cell, sample)) in raw_cells.iter().zip(samples).enumerate() {
                    let RawPropertyCategory::Ishmael(value) = &cell.categories[category_index]
                    else {
                        return Err(RawPropertyBlendError::CategoryLayoutMismatch { sample_index });
                    };
                    if value.category != output.category
                        || value.source_names != output.source_names
                    {
                        return Err(RawPropertyBlendError::CategoryLayoutMismatch { sample_index });
                    }
                    output.qice_kgkg += sample.weight * value.qice_kgkg;
                    output.qnice_per_kg += sample.weight * value.qnice_per_kg;
                    output.qvoli_m3_per_kg += sample.weight * value.qvoli_m3_per_kg;
                    output.qaoli_m3_per_kg += sample.weight * value.qaoli_m3_per_kg;
                }
                output.diagnostics = IshmaelDiagnostics::new(
                    blend_ishmael_diagnostic(&raw_cells, samples, category_index, |value| {
                        value.diagnostics.d_ice_m()
                    }),
                    blend_ishmael_diagnostic(&raw_cells, samples, category_index, |value| {
                        value.diagnostics.rho_ice_kg_m3()
                    }),
                    blend_ishmael_diagnostic(&raw_cells, samples, category_index, |value| {
                        value.diagnostics.phi_ice()
                    }),
                    blend_ishmael_diagnostic(&raw_cells, samples, category_index, |value| {
                        value.diagnostics.v_ice_m_s()
                    }),
                );
                categories.push(RawPropertyCategory::Ishmael(output));
            }
        }
    }

    let rain = match &first.rain {
        RawRainState::Available { .. } => {
            let mut qrain_kgkg = 0.0;
            let mut qnrain_per_kg = 0.0;
            for (sample_index, (cell, sample)) in raw_cells.iter().zip(samples).enumerate() {
                let RawRainState::Available {
                    qrain_kgkg: qrain,
                    qnrain_per_kg: qnrain,
                } = &cell.rain
                else {
                    return Err(RawPropertyBlendError::RainAvailabilityMismatch { sample_index });
                };
                qrain_kgkg += sample.weight * qrain;
                qnrain_per_kg += sample.weight * qnrain;
            }
            RawRainState::Available {
                qrain_kgkg,
                qnrain_per_kg,
            }
        }
        RawRainState::Unavailable(first_reason) => {
            for (sample_index, cell) in raw_cells.iter().enumerate() {
                if !matches!(&cell.rain, RawRainState::Unavailable(reason) if reason == first_reason)
                {
                    return Err(RawPropertyBlendError::RainAvailabilityMismatch { sample_index });
                }
            }
            RawRainState::Unavailable(first_reason.clone())
        }
    };

    Ok(RawPropertyCell {
        source_cell_index: None,
        microphysics_scheme_id: first.microphysics_scheme_id,
        required_field_signature: Arc::clone(&first.required_field_signature),
        environment,
        categories,
        rain,
    })
}

fn blend_ishmael_diagnostic(
    raw_cells: &[RawPropertyCell],
    samples: &[WeightedRawPropertyCell<'_>],
    category_index: usize,
    value: impl Fn(&RawIshmaelCategory) -> Option<f64>,
) -> Option<f64> {
    let mut weighted_value = 0.0;
    let mut weighted_mass = 0.0;
    for (cell, sample) in raw_cells.iter().zip(samples) {
        let RawPropertyCategory::Ishmael(category) = &cell.categories[category_index] else {
            return None;
        };
        if sample.weight == 0.0 || category.qice_kgkg == 0.0 {
            continue;
        }
        let diagnostic = value(category)?;
        let mass_weight = sample.weight * category.qice_kgkg;
        weighted_value += mass_weight * diagnostic;
        weighted_mass += mass_weight;
    }
    (weighted_mass > 0.0).then_some(weighted_value / weighted_mass)
}

/// Apply nonlinear property closure only after raw spatial/temporal blending.
pub fn close_raw_property_cell(
    raw: &RawPropertyCell,
    orientation: OrientationDefinition,
) -> Result<ClosedPropertyCell, RawPropertyClosureError> {
    let context = ClosureContext::with_environment(raw.microphysics_scheme_id, raw.environment)
        .map_err(|source| RawPropertyClosureError::Environment { source })?
        .with_orientation(orientation);
    let mut categories = Vec::new();
    for category in &raw.categories {
        if category.mixing_ratio_kgkg() == 0.0 {
            continue;
        }
        let (identity, source_fields, closed) = match category {
            RawPropertyCategory::P3(value) => {
                let identity = WrfPropertyCategory::P3(value.category);
                let input = P3CategoryInput::from_optional(
                    value.category,
                    Some(value.qice_kgkg),
                    Some(value.qnice_per_kg),
                    Some(value.qir_kgkg),
                    Some(value.qib_m3_per_kg),
                    value.qzi,
                );
                let names = match value.category {
                    P3Category::Category1 => &["QICE", "QNICE", "QIR", "QIB"][..],
                    P3Category::Category2 => &["QICE2", "QNICE2", "QIR2", "QIB2"][..],
                };
                let mut source_fields = names.to_vec();
                if value.qzi.is_some() {
                    source_fields.push("QZI");
                }
                let closed = close_p3_category(&context, &input).map_err(|source| {
                    RawPropertyClosureError::Category {
                        category: identity,
                        source,
                    }
                })?;
                (identity, source_fields, closed)
            }
            RawPropertyCategory::Ishmael(value) => {
                let ishmael_category = value
                    .category
                    .ishmael_category()
                    .expect("raw ISHMAEL category has ISHMAEL identity");
                let input = IshmaelCategoryInput::from_optional(
                    ishmael_category,
                    Some(value.qice_kgkg),
                    Some(value.qnice_per_kg),
                    Some(value.qvoli_m3_per_kg),
                    Some(value.qaoli_m3_per_kg),
                    value.diagnostics,
                )
                .with_source_fields(value.source_names);
                let mut source_fields = value.source_names.required().to_vec();
                for (present, name) in [
                    (
                        value.diagnostics.d_ice_m().is_some(),
                        value.source_names.diagnostics()[0],
                    ),
                    (
                        value.diagnostics.rho_ice_kg_m3().is_some(),
                        value.source_names.diagnostics()[1],
                    ),
                    (
                        value.diagnostics.phi_ice().is_some(),
                        value.source_names.diagnostics()[2],
                    ),
                    (
                        value.diagnostics.v_ice_m_s().is_some(),
                        value.source_names.diagnostics()[3],
                    ),
                ] {
                    if present {
                        source_fields.push(name);
                    }
                }
                let closed = close_ishmael_category(&context, &input).map_err(|source| {
                    RawPropertyClosureError::Category {
                        category: value.category,
                        source,
                    }
                })?;
                (value.category, source_fields, closed)
            }
        };
        categories.push(ClosedCellCategory {
            category: identity,
            source_fields,
            closed,
        });
    }
    let rain = match &raw.rain {
        RawRainState::Unavailable(reason) => ClosedRainState::Unavailable(reason.clone()),
        RawRainState::Available { qrain_kgkg, .. } if *qrain_kgkg == 0.0 => ClosedRainState::Clear,
        RawRainState::Available {
            qrain_kgkg,
            qnrain_per_kg,
        } => {
            let input = ConventionalCategoryInput::new(
                ConventionalHydrometeor::Rain,
                *qrain_kgkg,
                Some(*qnrain_per_kg),
            );
            ClosedRainState::Closed(Box::new(
                close_conventional_category(&context, &input)
                    .map_err(|source| RawPropertyClosureError::Rain { source })?,
            ))
        }
    };
    Ok(ClosedPropertyCell {
        source_cell_index: raw.source_cell_index,
        environment: raw.environment,
        categories,
        rain,
    })
}

/// Retained byte estimate.  It excludes allocator slack and temporary raw
/// fields, and includes every logical heap element owned by the scene.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PropertyMemoryEstimate {
    pub structure_bytes: usize,
    pub index_bytes: usize,
    pub value_bytes: usize,
    pub identity_bytes: usize,
    pub provenance_text_bytes: usize,
}

impl PropertyMemoryEstimate {
    #[must_use]
    pub const fn retained_bytes(self) -> usize {
        self.structure_bytes
            .saturating_add(self.index_bytes)
            .saturating_add(self.value_bytes)
            .saturating_add(self.identity_bytes)
            .saturating_add(self.provenance_text_bytes)
    }
}

/// One category closed at a concrete cell, retaining the exact source tuple.
#[derive(Clone, Debug, PartialEq)]
pub struct ClosedCellCategory {
    category: WrfPropertyCategory,
    source_fields: Vec<&'static str>,
    closed: ClosedParticleCategory,
}

impl ClosedCellCategory {
    #[must_use]
    pub const fn category(&self) -> WrfPropertyCategory {
        self.category
    }

    #[must_use]
    pub fn source_fields(&self) -> &[&'static str] {
        &self.source_fields
    }

    #[must_use]
    pub const fn closed(&self) -> &ClosedParticleCategory {
        &self.closed
    }
}

/// Property closure at one active WRF cell.
#[derive(Clone, Debug, PartialEq)]
pub struct ClosedPropertyCell {
    source_cell_index: Option<u32>,
    environment: ParticleEnvironment,
    categories: Vec<ClosedCellCategory>,
    rain: ClosedRainState,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClosedRainState {
    Clear,
    Closed(Box<ClosedParticleCategory>),
    Unavailable(RainUnavailableReason),
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum CoexistenceUnavailable {
    #[error("diagnostic coexistence has no positive rain at this gate")]
    NoRainMass,
    #[error("diagnostic coexistence rain PSD is unavailable: {0}")]
    RainUnavailable(RainUnavailableReason),
    #[error(transparent)]
    Closure(#[from] ClosureError),
}

impl ClosedPropertyCell {
    #[must_use]
    pub const fn source_cell_index(&self) -> Option<u32> {
        self.source_cell_index
    }

    #[must_use]
    pub const fn environment(&self) -> ParticleEnvironment {
        self.environment
    }

    #[must_use]
    pub fn categories(&self) -> &[ClosedCellCategory] {
        &self.categories
    }

    #[must_use]
    pub const fn rain(&self) -> &ClosedRainState {
        &self.rain
    }

    /// Diagnose melting while preserving the unpaired rain remainder as one
    /// separate scattering component.  Paired liquid is represented only in
    /// the wet categories and is never yielded again as rain.
    pub fn diagnose_coexistence(
        &self,
        topology: MixtureTopology,
    ) -> Result<CoexistencePartition, CoexistenceUnavailable> {
        let rain = match &self.rain {
            ClosedRainState::Clear => return Err(CoexistenceUnavailable::NoRainMass),
            ClosedRainState::Unavailable(reason) => {
                return Err(CoexistenceUnavailable::RainUnavailable(reason.clone()));
            }
            ClosedRainState::Closed(rain) => rain.as_ref().clone(),
        };
        let original_rain = rain.clone();
        let frozen = self
            .categories
            .iter()
            .map(|category| category.closed.clone())
            .collect();
        let diagnosis =
            DiagnosticCoexistenceInput::new(self.environment.temperature_k(), rain, frozen)?
                .with_topology(topology)
                .diagnose()?;
        Ok(CoexistencePartition {
            diagnosis,
            original_rain,
        })
    }
}

/// Explicit mass components for the future LUT/renderer seam.
#[derive(Clone, Copy, Debug)]
pub enum CoexistenceScatteringComponent<'a> {
    WetCategory(&'a DiagnosticWetCategory),
    UnusedRain {
        source: &'a ClosedParticleCategory,
        mixing_ratio_kgkg: f64,
    },
}

/// Diagnostic coexistence with a non-duplicating component view.
#[derive(Clone, Debug, PartialEq)]
pub struct CoexistencePartition {
    diagnosis: DiagnosticCoexistenceResult,
    original_rain: ClosedParticleCategory,
}

impl CoexistencePartition {
    #[must_use]
    pub const fn diagnosis(&self) -> &DiagnosticCoexistenceResult {
        &self.diagnosis
    }

    #[must_use]
    pub fn scattering_components(&self) -> Vec<CoexistenceScatteringComponent<'_>> {
        let mut components = self
            .diagnosis
            .wet_categories()
            .iter()
            .map(CoexistenceScatteringComponent::WetCategory)
            .collect::<Vec<_>>();
        if self.diagnosis.unused_rain_mass_kgkg() > 0.0 {
            components.push(CoexistenceScatteringComponent::UnusedRain {
                source: &self.original_rain,
                mixing_ratio_kgkg: self.diagnosis.unused_rain_mass_kgkg(),
            });
        }
        components
    }

    #[must_use]
    pub fn accounted_scattering_mass_kgkg(&self) -> f64 {
        self.scattering_components()
            .into_iter()
            .map(|component| match component {
                CoexistenceScatteringComponent::WetCategory(category) => {
                    category.wet_total_mass_kgkg()
                }
                CoexistenceScatteringComponent::UnusedRain {
                    mixing_ratio_kgkg, ..
                } => mixing_ratio_kgkg,
            })
            .sum()
    }
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum WrfPropertyReadError {
    #[error("read WRF microphysics scheme id: {message}")]
    SchemeId { message: String },
    #[error("WRF mp_physics={scheme_id} has no P3/ISHMAEL property reader")]
    UnsupportedScheme { scheme_id: i32 },
    #[error("WRF time index {time_index} is outside {time_count} records")]
    TimeOutOfRange {
        time_index: usize,
        time_count: usize,
    },
    #[error("WRF property grid has {cell_count} cells; compact u32 indices cannot represent it")]
    GridTooLarge { cell_count: usize },
    #[error("required WRF field {field} for {category} is absent")]
    MissingRequiredField {
        category: WrfPropertyCategory,
        field: &'static str,
    },
    #[error("required WRF environment field {field} is absent")]
    MissingEnvironmentField { field: &'static str },
    #[error("required WRF environment field {field} is missing at cell {cell_index}")]
    MissingEnvironmentValue {
        field: &'static str,
        cell_index: usize,
    },
    #[error("read WRF field {field}: {message}")]
    FieldRead {
        field: &'static str,
        message: String,
    },
    #[error("WRF field {field} has {actual} cells, expected {expected}")]
    FieldShape {
        field: &'static str,
        actual: usize,
        expected: usize,
    },
    #[error("WRF field {field} unit {source_units:?} cannot normalize to {expected}")]
    UnsupportedUnit {
        field: &'static str,
        source_units: String,
        expected: &'static str,
    },
    #[error("WRF mass field {field} is missing at cell {cell_index}")]
    MissingMassValue {
        field: &'static str,
        cell_index: usize,
    },
    #[error("WRF mass field {field} is negative at cell {cell_index}: {value}")]
    NegativeMassValue {
        field: &'static str,
        cell_index: usize,
        value: f64,
    },
    #[error("required WRF field {field} for {category} is missing at active cell {cell_index}")]
    MissingRequiredValue {
        category: WrfPropertyCategory,
        field: &'static str,
        cell_index: usize,
    },
    #[error(
        "normalized WRF field {field} value {value} cannot be stored as finite f32 at cell {cell_index}"
    )]
    ValueNotRepresentable {
        field: &'static str,
        cell_index: usize,
        value: f64,
    },
    #[error("invalid WRF environment at cell {cell_index}: {reason}")]
    InvalidEnvironment {
        cell_index: usize,
        reason: &'static str,
    },
    #[error("cell {cell_index} is outside the {cell_count}-cell WRF grid")]
    CellOutOfRange {
        cell_index: usize,
        cell_count: usize,
    },
    #[error("close WRF environment at cell {cell_index}: {source}")]
    EnvironmentClosure {
        cell_index: usize,
        #[source]
        source: ClosureError,
    },
    #[error("close {category} at cell {cell_index}: {source}")]
    CategoryClosure {
        cell_index: usize,
        category: WrfPropertyCategory,
        #[source]
        source: ClosureError,
    },
    #[error("close rain at cell {cell_index}: {source}")]
    RainClosure {
        cell_index: usize,
        #[source]
        source: ClosureError,
    },
}

/// Inventory and normalize one WRF model time through a provider.
pub fn read_property_scene<P: PropertyFieldProvider + ?Sized>(
    provider: &P,
    time_index: usize,
) -> Result<WrfPropertyScene, WrfPropertyReadError> {
    let _cache_scope = ProviderCacheScope(provider);
    provider.clear_cache();

    let scheme_id = provider
        .microphysics_scheme_id()
        .map_err(|message| WrfPropertyReadError::SchemeId { message })?;
    if !matches!(scheme_id, 50..=53 | 55) {
        return Err(WrfPropertyReadError::UnsupportedScheme { scheme_id });
    }
    let time_count = provider.time_count();
    if time_index >= time_count {
        return Err(WrfPropertyReadError::TimeOutOfRange {
            time_index,
            time_count,
        });
    }
    let cell_count = provider.cell_count();
    if (cell_count as u128) > (u128::from(u32::MAX) + 1) {
        return Err(WrfPropertyReadError::GridTooLarge { cell_count });
    }

    let required_field_signature = inventory_field_signature(provider, scheme_id);
    let mut categories = Vec::new();
    let mut skipped_zero_mass_categories = Vec::new();
    let mut source_fields = Vec::new();
    let mut ishmael_source_names = Vec::new();

    if matches!(scheme_id, 50..=53) {
        let mut first = P3_CATEGORY_1;
        if scheme_id == 53 {
            first.qzi = Some(QZI_FIELD);
        }
        let specs: &[P3Spec] = if scheme_id == 52 {
            &[first, P3_CATEGORY_2]
        } else {
            std::slice::from_ref(&first)
        };
        for &spec in specs {
            let category = WrfPropertyCategory::P3(spec.category);
            let (active_cell_indices, qice_kgkg, mass_provenance) =
                read_mass_field(provider, time_index, cell_count, spec.qice, category)?;
            source_fields.push(mass_provenance);
            if active_cell_indices.is_empty() {
                skipped_zero_mass_categories.push(category);
                continue;
            }

            let (qnice_per_kg, qnice_provenance) = read_required_category_field(
                provider,
                time_index,
                cell_count,
                spec.qnice,
                category,
                &active_cell_indices,
            )?;
            let (qir_kgkg, qir_provenance) = read_required_category_field(
                provider,
                time_index,
                cell_count,
                spec.qir,
                category,
                &active_cell_indices,
            )?;
            let (qib_m3_per_kg, qib_provenance) = read_required_category_field(
                provider,
                time_index,
                cell_count,
                spec.qib,
                category,
                &active_cell_indices,
            )?;
            source_fields.extend([qnice_provenance, qir_provenance, qib_provenance]);
            let (qzi, qzi_name) = if let Some(qzi_spec) = spec.qzi {
                let (values, provenance) = read_required_category_field(
                    provider,
                    time_index,
                    cell_count,
                    qzi_spec,
                    category,
                    &active_cell_indices,
                )?;
                let name = provenance.source_name;
                source_fields.push(provenance);
                (Some(values), Some(name))
            } else {
                (None, None)
            };
            let mut category_sources = vec![
                spec.qice.name,
                spec.qnice.name,
                spec.qir.name,
                spec.qib.name,
            ];
            category_sources.extend(qzi_name);
            categories.push(SparsePropertyCategory {
                category,
                active_cell_indices,
                source_fields: category_sources,
                values: SparseCategoryValues::P3 {
                    qice_kgkg,
                    qnice_per_kg,
                    qir_kgkg,
                    qib_m3_per_kg,
                    qzi,
                },
            });
        }
    } else {
        for spec in ISHMAEL_SPECS {
            let category = spec.category;
            let tuple_source_names = resolved_ishmael_source_names(provider, spec);
            ishmael_source_names.push((category, tuple_source_names));
            let (active_cell_indices, qice_kgkg, mass_provenance) =
                read_mass_field(provider, time_index, cell_count, spec.qice, category)?;
            source_fields.push(mass_provenance);
            if active_cell_indices.is_empty() {
                skipped_zero_mass_categories.push(category);
                continue;
            }
            let (qnice_per_kg, qnice_provenance) = read_required_category_field(
                provider,
                time_index,
                cell_count,
                spec.qnice,
                category,
                &active_cell_indices,
            )?;
            let (qvoli_m3_per_kg, qvoli_provenance) = read_required_category_field(
                provider,
                time_index,
                cell_count,
                spec.qvoli,
                category,
                &active_cell_indices,
            )?;
            let (qaoli_m3_per_kg, qaoli_provenance) = read_required_category_field(
                provider,
                time_index,
                cell_count,
                spec.qaoli,
                category,
                &active_cell_indices,
            )?;
            source_fields.extend([qnice_provenance, qvoli_provenance, qaoli_provenance]);

            let mut category_sources = vec![
                spec.qice.name,
                spec.qnice.name,
                spec.qvoli.name,
                spec.qaoli.name,
            ];
            let (d_ice, d_name) = read_optional_diagnostic(
                provider,
                time_index,
                cell_count,
                spec.d_ice,
                &active_cell_indices,
            )?;
            let (rho_ice, rho_name) = read_optional_diagnostic(
                provider,
                time_index,
                cell_count,
                spec.rho_ice,
                &active_cell_indices,
            )?;
            let (phi_ice, phi_name) = read_optional_diagnostic(
                provider,
                time_index,
                cell_count,
                spec.phi_ice,
                &active_cell_indices,
            )?;
            let (v_ice, v_name) = read_optional_diagnostic(
                provider,
                time_index,
                cell_count,
                spec.v_ice,
                &active_cell_indices,
            )?;
            for read in [&d_ice, &rho_ice, &phi_ice, &v_ice].into_iter().flatten() {
                source_fields.push(read.provenance.clone());
            }
            category_sources.extend([d_name, rho_name, phi_name, v_name].into_iter().flatten());
            categories.push(SparsePropertyCategory {
                category,
                active_cell_indices,
                source_fields: category_sources,
                values: SparseCategoryValues::Ishmael {
                    qice_kgkg,
                    qnice_per_kg,
                    qvoli_m3_per_kg,
                    qaoli_m3_per_kg,
                    diagnostics: Box::new(IshmaelDiagnosticStorage {
                        d_ice: d_ice.map(|read| read.field),
                        rho_ice: rho_ice.map(|read| read.field),
                        phi_ice: phi_ice.map(|read| read.field),
                        v_ice: v_ice.map(|read| read.field),
                    }),
                    source_names: tuple_source_names,
                },
            });
        }
    }

    let active_cell_indices = union_active_cells(&categories);
    let (rain, rain_provenance) = read_rain(provider, time_index, cell_count);
    source_fields.extend(rain_provenance);
    let (environment, environment_provenance) = read_environment(provider, time_index, cell_count)?;
    source_fields.extend(environment_provenance);

    Ok(WrfPropertyScene {
        identity: PropertySceneIdentity {
            source_identity: provider.source_identity(),
            time_index,
        },
        microphysics_scheme_id: scheme_id,
        cell_count,
        active_cell_indices,
        environment,
        categories,
        rain,
        skipped_zero_mass_categories,
        ishmael_source_names,
        source_fields,
        required_field_signature: Arc::new(required_field_signature),
    })
}

struct ProviderCacheScope<'a, P: PropertyFieldProvider + ?Sized>(&'a P);

impl<P: PropertyFieldProvider + ?Sized> Drop for ProviderCacheScope<'_, P> {
    fn drop(&mut self) {
        self.0.clear_cache();
    }
}

fn inventory_field_signature<P: PropertyFieldProvider + ?Sized>(
    provider: &P,
    scheme_id: i32,
) -> RequiredFieldSignature {
    let mut fields = BTreeSet::new();
    let mut add_if_present = |spec: FieldSpec| {
        if let Some(resolved) = resolve_field(provider, spec) {
            fields.insert(resolved.contract());
        }
    };
    if matches!(scheme_id, 50..=53) {
        let mut first = P3_CATEGORY_1;
        if scheme_id == 53 {
            first.qzi = Some(QZI_FIELD);
        }
        let specs: &[P3Spec] = if scheme_id == 52 {
            &[first, P3_CATEGORY_2]
        } else {
            std::slice::from_ref(&first)
        };
        for spec in specs {
            for field in [
                Some(spec.qice),
                Some(spec.qnice),
                Some(spec.qir),
                Some(spec.qib),
                spec.qzi,
            ]
            .into_iter()
            .flatten()
            {
                add_if_present(field);
            }
        }
    } else {
        for spec in ISHMAEL_SPECS {
            for field in [
                spec.qice,
                spec.qnice,
                spec.qvoli,
                spec.qaoli,
                spec.d_ice,
                spec.rho_ice,
                spec.phi_ice,
                spec.v_ice,
            ] {
                add_if_present(field);
            }
        }
    }
    for field in [T_FIELD, P_FIELD, PB_FIELD, QVAPOR_FIELD] {
        add_if_present(field);
    }
    for field in [QRAIN_FIELD, QNRAIN_FIELD] {
        add_if_present(field);
    }
    RequiredFieldSignature {
        microphysics_scheme_id: scheme_id,
        fields,
    }
}

fn read_mass_field<P: PropertyFieldProvider + ?Sized>(
    provider: &P,
    time_index: usize,
    cell_count: usize,
    spec: FieldSpec,
    category: WrfPropertyCategory,
) -> Result<(Vec<u32>, Vec<f32>, SourceFieldProvenance), WrfPropertyReadError> {
    if !provider.has_field(spec.name) {
        return Err(WrfPropertyReadError::MissingRequiredField {
            category,
            field: spec.name,
        });
    }
    let normalized = read_normalized_field(provider, time_index, cell_count, spec)?;
    let mut indices = Vec::new();
    let mut values = Vec::new();
    for (cell_index, value) in normalized.values.iter().copied().enumerate() {
        if is_missing(value) {
            return Err(WrfPropertyReadError::MissingMassValue {
                field: spec.name,
                cell_index,
            });
        }
        if value < 0.0 {
            return Err(WrfPropertyReadError::NegativeMassValue {
                field: spec.name,
                cell_index,
                value,
            });
        }
        if value == 0.0 {
            continue;
        }
        indices.push(u32::try_from(cell_index).expect("grid size checked before reads"));
        values.push(narrow_f32(spec.name, cell_index, value)?);
    }
    Ok((indices, values, normalized.provenance))
}

fn read_required_category_field<P: PropertyFieldProvider + ?Sized>(
    provider: &P,
    time_index: usize,
    cell_count: usize,
    spec: FieldSpec,
    category: WrfPropertyCategory,
    active_cell_indices: &[u32],
) -> Result<(Vec<f32>, SourceFieldProvenance), WrfPropertyReadError> {
    if !provider.has_field(spec.name) {
        return Err(WrfPropertyReadError::MissingRequiredField {
            category,
            field: spec.name,
        });
    }
    let normalized = read_normalized_field(provider, time_index, cell_count, spec)?;
    let values = compact_required_values(
        &normalized.values,
        active_cell_indices,
        spec,
        |cell_index| WrfPropertyReadError::MissingRequiredValue {
            category,
            field: spec.name,
            cell_index,
        },
    )?;
    Ok((values, normalized.provenance))
}

struct OptionalDiagnosticRead {
    field: SparseDiagnostic,
    provenance: SourceFieldProvenance,
}

fn read_optional_diagnostic<P: PropertyFieldProvider + ?Sized>(
    provider: &P,
    time_index: usize,
    cell_count: usize,
    semantic_spec: FieldSpec,
    active_cell_indices: &[u32],
) -> Result<(Option<OptionalDiagnosticRead>, Option<&'static str>), WrfPropertyReadError> {
    let Some(spec) = resolve_field(provider, semantic_spec) else {
        return Ok((None, None));
    };
    let normalized = read_normalized_field(provider, time_index, cell_count, spec)?;
    let mut indices = Vec::new();
    let mut values = Vec::new();
    for &cell_index in active_cell_indices {
        let position = cell_index as usize;
        let value = normalized.values[position];
        if is_fill_value(value) {
            continue;
        }
        indices.push(cell_index);
        values.push(narrow_f32(spec.name, position, value)?);
    }
    Ok((
        Some(OptionalDiagnosticRead {
            field: SparseDiagnostic {
                cell_indices: indices,
                values,
            },
            provenance: normalized.provenance,
        }),
        Some(spec.name),
    ))
}

fn read_rain<P: PropertyFieldProvider + ?Sized>(
    provider: &P,
    time_index: usize,
    cell_count: usize,
) -> (SparseRainStorage, Vec<SourceFieldProvenance>) {
    if !provider.has_field(QRAIN_FIELD.name) {
        return (
            SparseRainStorage::Unavailable(RainUnavailableReason::MissingMassField {
                field: QRAIN_FIELD.name,
            }),
            Vec::new(),
        );
    }
    let mass = match read_normalized_field(provider, time_index, cell_count, QRAIN_FIELD) {
        Ok(value) => value,
        Err(error) => {
            return (
                SparseRainStorage::Unavailable(RainUnavailableReason::InvalidField {
                    field: QRAIN_FIELD.name,
                    message: error.to_string(),
                }),
                Vec::new(),
            );
        }
    };
    let mut provenance = vec![mass.provenance];
    let mut active_cell_indices = Vec::new();
    let mut qrain_kgkg = Vec::new();
    for (cell_index, value) in mass.values.into_iter().enumerate() {
        if is_missing(value) || value < 0.0 {
            return (
                SparseRainStorage::Unavailable(RainUnavailableReason::InvalidField {
                    field: QRAIN_FIELD.name,
                    message: format!("invalid mass {value} at cell {cell_index}"),
                }),
                provenance,
            );
        }
        if value > 0.0 {
            let compact = match narrow_f32(QRAIN_FIELD.name, cell_index, value) {
                Ok(value) => value,
                Err(error) => {
                    return (
                        SparseRainStorage::Unavailable(RainUnavailableReason::InvalidField {
                            field: QRAIN_FIELD.name,
                            message: error.to_string(),
                        }),
                        provenance,
                    );
                }
            };
            active_cell_indices
                .push(u32::try_from(cell_index).expect("grid size checked before rain read"));
            qrain_kgkg.push(compact);
        }
    }
    if !provider.has_field(QNRAIN_FIELD.name) {
        return (
            SparseRainStorage::Unavailable(RainUnavailableReason::MissingNumberField {
                field: QNRAIN_FIELD.name,
            }),
            provenance,
        );
    }
    let number = match read_normalized_field(provider, time_index, cell_count, QNRAIN_FIELD) {
        Ok(value) => value,
        Err(error) => {
            return (
                SparseRainStorage::Unavailable(RainUnavailableReason::InvalidField {
                    field: QNRAIN_FIELD.name,
                    message: error.to_string(),
                }),
                provenance,
            );
        }
    };
    provenance.push(number.provenance);
    let mut qnrain_per_kg = Vec::with_capacity(active_cell_indices.len());
    for &cell_index in &active_cell_indices {
        let position = cell_index as usize;
        let value = number.values[position];
        if is_missing(value) || value <= 0.0 {
            return (
                SparseRainStorage::Unavailable(RainUnavailableReason::InvalidField {
                    field: QNRAIN_FIELD.name,
                    message: format!("invalid number {value} at rainy cell {position}"),
                }),
                provenance,
            );
        }
        match narrow_f32(QNRAIN_FIELD.name, position, value) {
            Ok(value) => qnrain_per_kg.push(value),
            Err(error) => {
                return (
                    SparseRainStorage::Unavailable(RainUnavailableReason::InvalidField {
                        field: QNRAIN_FIELD.name,
                        message: error.to_string(),
                    }),
                    provenance,
                );
            }
        }
    }
    (
        SparseRainStorage::Available {
            active_cell_indices,
            qrain_kgkg,
            qnrain_per_kg,
        },
        provenance,
    )
}

fn read_environment<P: PropertyFieldProvider + ?Sized>(
    provider: &P,
    time_index: usize,
    cell_count: usize,
) -> Result<(DenseEnvironment, Vec<SourceFieldProvenance>), WrfPropertyReadError> {
    let (mut temperature_k, t_provenance) =
        read_dense_environment_source(provider, time_index, cell_count, T_FIELD)?;
    let (mut air_density_kg_m3, p_provenance) =
        read_dense_environment_source(provider, time_index, cell_count, P_FIELD)?;
    let (base_pressure, pb_provenance) =
        read_dense_environment_source(provider, time_index, cell_count, PB_FIELD)?;
    for (pressure, base) in air_density_kg_m3.iter_mut().zip(base_pressure) {
        *pressure += base;
    }
    for cell_index in 0..cell_count {
        let pressure = f64::from(air_density_kg_m3[cell_index]);
        let theta = f64::from(temperature_k[cell_index]) + 300.0;
        if !pressure.is_finite() || pressure <= 0.0 {
            return Err(WrfPropertyReadError::InvalidEnvironment {
                cell_index,
                reason: "P + PB must be finite and positive",
            });
        }
        if !theta.is_finite() || theta <= 0.0 {
            return Err(WrfPropertyReadError::InvalidEnvironment {
                cell_index,
                reason: "T + 300 must be finite and positive",
            });
        }
        let temperature = theta * (pressure / WRF_REFERENCE_PRESSURE_PA).powf(WRF_KAPPA);
        if !temperature.is_finite() || temperature <= 0.0 {
            return Err(WrfPropertyReadError::InvalidEnvironment {
                cell_index,
                reason: "derived temperature must be finite and positive",
            });
        }
        temperature_k[cell_index] = narrow_f32("temperature", cell_index, temperature)?;
    }
    let (water_vapor, qv_provenance) =
        read_dense_environment_source(provider, time_index, cell_count, QVAPOR_FIELD)?;
    for cell_index in 0..cell_count {
        let pressure = f64::from(air_density_kg_m3[cell_index]);
        let temperature = f64::from(temperature_k[cell_index]);
        let qv = f64::from(water_vapor[cell_index]);
        let virtual_temperature = temperature * (1.0 + 0.61 * qv.max(0.0));
        let density = pressure / (DRY_AIR_GAS_CONSTANT_J_KG_K * virtual_temperature);
        if !density.is_finite() || density <= 0.0 {
            return Err(WrfPropertyReadError::InvalidEnvironment {
                cell_index,
                reason: "derived moist-air density must be finite and positive",
            });
        }
        air_density_kg_m3[cell_index] = narrow_f32("air_density", cell_index, density)?;
    }
    Ok((
        DenseEnvironment {
            temperature_k,
            air_density_kg_m3,
        },
        vec![t_provenance, p_provenance, pb_provenance, qv_provenance],
    ))
}

fn read_dense_environment_source<P: PropertyFieldProvider + ?Sized>(
    provider: &P,
    time_index: usize,
    cell_count: usize,
    spec: FieldSpec,
) -> Result<(Vec<f32>, SourceFieldProvenance), WrfPropertyReadError> {
    if !provider.has_field(spec.name) {
        return Err(WrfPropertyReadError::MissingEnvironmentField { field: spec.name });
    }
    let normalized = read_normalized_field(provider, time_index, cell_count, spec)?;
    let mut values = Vec::with_capacity(cell_count);
    for (cell_index, value) in normalized.values.into_iter().enumerate() {
        if is_missing(value) {
            return Err(WrfPropertyReadError::MissingEnvironmentValue {
                field: spec.name,
                cell_index,
            });
        }
        values.push(narrow_f32(spec.name, cell_index, value)?);
    }
    Ok((values, normalized.provenance))
}

fn union_active_cells(categories: &[SparsePropertyCategory]) -> Vec<u32> {
    let mut union = Vec::new();
    for category in categories {
        union = merge_sorted_unique(&union, &category.active_cell_indices);
    }
    union
}

fn merge_sorted_unique(left: &[u32], right: &[u32]) -> Vec<u32> {
    let mut result = Vec::with_capacity(left.len() + right.len());
    let (mut left_position, mut right_position) = (0, 0);
    while left_position < left.len() || right_position < right.len() {
        let next = match (left.get(left_position), right.get(right_position)) {
            (Some(&left_value), Some(&right_value)) if left_value < right_value => {
                left_position += 1;
                left_value
            }
            (Some(&left_value), Some(&right_value)) if right_value < left_value => {
                right_position += 1;
                right_value
            }
            (Some(&value), Some(_)) => {
                left_position += 1;
                right_position += 1;
                value
            }
            (Some(&value), None) => {
                left_position += 1;
                value
            }
            (None, Some(&value)) => {
                right_position += 1;
                value
            }
            (None, None) => break,
        };
        result.push(next);
    }
    result
}

struct NormalizedRead {
    values: Vec<f64>,
    provenance: SourceFieldProvenance,
}

fn read_normalized_field<P: PropertyFieldProvider + ?Sized>(
    provider: &P,
    time_index: usize,
    cell_count: usize,
    spec: FieldSpec,
) -> Result<NormalizedRead, WrfPropertyReadError> {
    let read_result = provider.read_field(spec.name, time_index);
    provider.clear_cache();
    let mut raw = read_result.map_err(|message| WrfPropertyReadError::FieldRead {
        field: spec.name,
        message,
    })?;
    if raw.values.len() != cell_count {
        return Err(WrfPropertyReadError::FieldShape {
            field: spec.name,
            actual: raw.values.len(),
            expected: cell_count,
        });
    }
    let scale =
        unit_scale(spec.unit, &raw.units).ok_or_else(|| WrfPropertyReadError::UnsupportedUnit {
            field: spec.name,
            source_units: raw.units.clone(),
            expected: spec.unit.symbol(),
        })?;
    if scale != 1.0 {
        for value in &mut raw.values {
            if !is_missing(*value) {
                *value *= scale;
            }
        }
    }
    Ok(NormalizedRead {
        values: raw.values,
        provenance: SourceFieldProvenance {
            source_name: spec.name,
            source_units: raw.units,
            normalized_unit: spec.unit,
            property: spec.property,
        },
    })
}

fn compact_required_values(
    dense: &[f64],
    indices: &[u32],
    spec: FieldSpec,
    missing_error: impl Fn(usize) -> WrfPropertyReadError,
) -> Result<Vec<f32>, WrfPropertyReadError> {
    let mut values = Vec::with_capacity(indices.len());
    for &cell_index in indices {
        let position = cell_index as usize;
        let value = dense[position];
        if is_missing(value) {
            return Err(missing_error(position));
        }
        values.push(narrow_f32(spec.name, position, value)?);
    }
    Ok(values)
}

fn narrow_f32(
    field: &'static str,
    cell_index: usize,
    value: f64,
) -> Result<f32, WrfPropertyReadError> {
    let compact = value as f32;
    if !compact.is_finite() || (value != 0.0 && compact == 0.0) {
        return Err(WrfPropertyReadError::ValueNotRepresentable {
            field,
            cell_index,
            value,
        });
    }
    Ok(compact)
}

fn is_missing(value: f64) -> bool {
    !value.is_finite() || is_fill_value(value)
}

fn is_fill_value(value: f64) -> bool {
    value.is_finite() && value.abs() >= WRF_FILL_MAGNITUDE
}

fn resolve_field<P: PropertyFieldProvider + ?Sized>(
    provider: &P,
    semantic_spec: FieldSpec,
) -> Option<FieldSpec> {
    if let Some(actual_name) = lowercase_ishmael_diagnostic(semantic_spec.name)
        && provider.has_field(actual_name)
    {
        return Some(FieldSpec {
            name: actual_name,
            ..semantic_spec
        });
    }
    provider
        .has_field(semantic_spec.name)
        .then_some(semantic_spec)
}

fn resolved_ishmael_source_names<P: PropertyFieldProvider + ?Sized>(
    provider: &P,
    spec: IshmaelSpec,
) -> IshmaelSourceFields {
    let diagnostic_name = |field: FieldSpec| {
        resolve_field(provider, field)
            .map(|resolved| resolved.name)
            .unwrap_or(field.name)
    };
    IshmaelSourceFields::new(
        spec.qice.name,
        spec.qnice.name,
        spec.qvoli.name,
        spec.qaoli.name,
        diagnostic_name(spec.d_ice),
        diagnostic_name(spec.rho_ice),
        diagnostic_name(spec.phi_ice),
        diagnostic_name(spec.v_ice),
    )
}

fn lowercase_ishmael_diagnostic(name: &str) -> Option<&'static str> {
    match name {
        "D_ICE" => Some("d_ice"),
        "RHO_ICE" => Some("rho_ice"),
        "PHI_ICE" => Some("phi_ice"),
        "V_ICE" => Some("v_ice"),
        "D_ICE2" => Some("d_ice2"),
        "RHO_ICE2" => Some("rho_ice2"),
        "PHI_ICE2" => Some("phi_ice2"),
        "V_ICE2" => Some("v_ice2"),
        "D_ICE3" => Some("d_ice3"),
        "RHO_ICE3" => Some("rho_ice3"),
        "PHI_ICE3" => Some("phi_ice3"),
        "V_ICE3" => Some("v_ice3"),
        _ => None,
    }
}

fn unit_scale(unit: NormalizedUnit, source: &str) -> Option<f64> {
    let key = unit_key(source);
    match unit {
        NormalizedUnit::KilogramsPerKilogram => match key.as_str() {
            "kgkg-1" | "kg/kg" | "1" => Some(1.0),
            "gkg-1" | "g/kg" => Some(1.0e-3),
            _ => None,
        },
        NormalizedUnit::PerKilogram => match key.as_str() {
            "kg-1" | "#kg-1" | "1/kg" | "#/kg" => Some(1.0),
            _ => None,
        },
        NormalizedUnit::CubicMetersPerKilogram => match key.as_str() {
            "m3kg-1" | "m^3kg^-1" | "m3/kg" => Some(1.0),
            "cm3kg-1" | "cm^3kg^-1" | "cm3/kg" => Some(1.0e-6),
            _ => None,
        },
        NormalizedUnit::Meters => match key.as_str() {
            "m" => Some(1.0),
            "mm" => Some(1.0e-3),
            "cm" => Some(1.0e-2),
            _ => None,
        },
        NormalizedUnit::KilogramsPerCubicMeter => match key.as_str() {
            "kgm-3" | "kgm^-3" | "kg/m3" | "kg/m^3" => Some(1.0),
            "gcm-3" | "gcm^-3" | "g/cm3" | "g/cm^3" => Some(1_000.0),
            _ => None,
        },
        NormalizedUnit::MetersPerSecond => match key.as_str() {
            "ms-1" | "ms^-1" | "m/s" => Some(1.0),
            "cms-1" | "cms^-1" | "cm/s" => Some(1.0e-2),
            _ => None,
        },
        NormalizedUnit::Kelvin => (key == "k").then_some(1.0),
        NormalizedUnit::Pascal => match key.as_str() {
            "pa" => Some(1.0),
            "hpa" | "mb" => Some(100.0),
            _ => None,
        },
        NormalizedUnit::Dimensionless => matches!(key.as_str(), "" | "1" | "ratio").then_some(1.0),
    }
}

fn unit_key(unit: &str) -> String {
    unit.trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|character| !character.is_whitespace() && !matches!(character, '(' | ')'))
        .collect()
}

fn wrf_registry_units(name: &str) -> Option<&'static str> {
    match name {
        "QICE" | "QICE2" | "QICE3" | "QIR" | "QIR2" | "QVAPOR" | "QRAIN" => Some("kg kg-1"),
        "QNICE" | "QNICE2" | "QNICE3" | "QNRAIN" => Some("# kg-1"),
        "QIB" | "QIB2" | "QZI" | "QVOLI" | "QVOLI2" | "QVOLI3" | "QAOLI" | "QAOLI2" | "QAOLI3" => {
            Some("m(3) kg(-1)")
        }
        "T" => Some("K"),
        "P" | "PB" => Some("Pa"),
        "D_ICE" | "D_ICE2" | "D_ICE3" | "d_ice" | "d_ice2" | "d_ice3" => Some("m"),
        "RHO_ICE" | "RHO_ICE2" | "RHO_ICE3" | "rho_ice" | "rho_ice2" | "rho_ice3" => Some("kg m-3"),
        "PHI_ICE" | "PHI_ICE2" | "PHI_ICE3" | "phi_ice" | "phi_ice2" | "phi_ice3" => Some("1"),
        "V_ICE" | "V_ICE2" | "V_ICE3" | "v_ice" | "v_ice2" | "v_ice3" => Some("m s-1"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;

    use radar_scattering::{ParticleProvenance, ParticleState, SourceVariable};

    use super::*;

    struct TinyProvider {
        source_identity: WrfSourceIdentity,
        _private_path: String,
        scheme_id: i32,
        cells: usize,
        fields: BTreeMap<&'static str, RawPropertyField>,
        reads: RefCell<Vec<&'static str>>,
        cache_clears: Cell<usize>,
    }

    impl TinyProvider {
        fn new(scheme_id: i32, cells: usize) -> Self {
            Self {
                source_identity: WrfSourceIdentity("sha256:opaque-scene-id".to_owned()),
                _private_path: r"C:\Users\scientist\private\wrfout_d01".to_owned(),
                scheme_id,
                cells,
                fields: BTreeMap::new(),
                reads: RefCell::new(Vec::new()),
                cache_clears: Cell::new(0),
            }
        }

        fn field(mut self, name: &'static str, values: Vec<f64>, units: &str) -> Self {
            assert_eq!(values.len(), self.cells);
            self.fields
                .insert(name, RawPropertyField::new(values, units));
            self
        }

        fn environment(mut self, temperature_k: f64) -> Self {
            let cells = self.cells;
            self.fields.insert(
                "T",
                RawPropertyField::new(vec![temperature_k - 300.0; cells], "K"),
            );
            self.fields
                .insert("P", RawPropertyField::new(vec![0.0; cells], "Pa"));
            self.fields
                .insert("PB", RawPropertyField::new(vec![1_000.0; cells], "hPa"));
            self.fields
                .insert("QVAPOR", RawPropertyField::new(vec![0.0; cells], "kg kg-1"));
            self
        }
    }

    impl PropertyFieldProvider for TinyProvider {
        fn source_identity(&self) -> WrfSourceIdentity {
            self.source_identity.clone()
        }

        fn microphysics_scheme_id(&self) -> Result<i32, String> {
            Ok(self.scheme_id)
        }

        fn cell_count(&self) -> usize {
            self.cells
        }

        fn time_count(&self) -> usize {
            1
        }

        fn has_field(&self, name: &str) -> bool {
            self.fields.contains_key(name)
        }

        fn read_field(&self, name: &str, _time_index: usize) -> Result<RawPropertyField, String> {
            let (&stored_name, field) = self
                .fields
                .get_key_value(name)
                .ok_or_else(|| format!("{name} absent"))?;
            self.reads.borrow_mut().push(stored_name);
            Ok(field.clone())
        }

        fn clear_cache(&self) {
            self.cache_clears.set(self.cache_clears.get() + 1);
        }
    }

    fn p3_50_provider(scheme_id: i32, cells: usize, active: usize) -> TinyProvider {
        let mut qice = vec![0.0; cells];
        let mut qnice = vec![f64::NAN; cells];
        let mut qir = vec![f64::NAN; cells];
        let mut qib = vec![f64::NAN; cells];
        qice[active] = 0.1; // g kg-1 -> 1e-4 kg kg-1
        qnice[active] = 1.0e6;
        qir[active] = 0.04; // g kg-1 -> 4e-5 kg kg-1
        qib[active] = 1.0e-7;
        TinyProvider::new(scheme_id, cells)
            .field("QICE", qice, "g kg-1")
            .field("QNICE", qnice, "# kg-1")
            .field("QIR", qir, "g/kg")
            .field("QIB", qib, "m(3) kg(-1)")
            .environment(271.65)
    }

    fn p3_52_provider(temperature_k: f64) -> TinyProvider {
        TinyProvider::new(52, 4)
            .field("QICE", vec![0.0, 1.0e-4, 0.0, 2.0e-4], "kg kg-1")
            .field("QNICE", vec![0.0, 1.0e6, 0.0, 2.0e6], "kg-1")
            .field("QIR", vec![0.0, 2.0e-5, 0.0, 4.0e-5], "kg kg-1")
            .field("QIB", vec![0.0, 5.0e-8, 0.0, 1.0e-7], "m3 kg-1")
            .field("QICE2", vec![0.0, 0.0, 3.0e-4, 4.0e-4], "kg/kg")
            .field("QNICE2", vec![0.0, 0.0, 3.0e6, 4.0e6], "#/kg")
            .field("QIR2", vec![0.0, 0.0, 1.2e-4, 1.6e-4], "kg kg-1")
            .field("QIB2", vec![0.0, 0.0, 3.0e-7, 4.0e-7], "m^3 kg^-1")
            .field("QRAIN", vec![0.0, 0.0, 0.0, 8.0e-4], "kg kg-1")
            .field("QNRAIN", vec![0.0, 0.0, 0.0, 1.0e6], "# kg-1")
            .environment(temperature_k)
    }

    fn ishmael_provider() -> TinyProvider {
        let provider = TinyProvider::new(55, 3)
            .field("QICE", vec![1.0e-4, 0.0, 0.0], "kg kg-1")
            .field("QNICE", vec![1.0e6, 0.0, 0.0], "# kg-1")
            .field("QVOLI", vec![2.0e-7, 0.0, 0.0], "m(3) kg(-1)")
            .field("QAOLI", vec![1.4e-7, 0.0, 0.0], "m3 kg-1")
            .field("QICE2", vec![0.0, 2.0e-4, 0.0], "kg kg-1")
            .field("QNICE2", vec![0.0, 2.0e6, 0.0], "# kg-1")
            .field("QVOLI2", vec![0.0, 4.0e-7, 0.0], "m3 kg-1")
            .field("QAOLI2", vec![0.0, 2.8e-7, 0.0], "m3 kg-1")
            .field("QICE3", vec![0.0, 0.0, 3.0e-4], "kg kg-1")
            .field("QNICE3", vec![0.0, 0.0, 3.0e6], "# kg-1")
            .field("QVOLI3", vec![0.0, 0.0, 6.0e-7], "m3 kg-1")
            .field("QAOLI3", vec![0.0, 0.0, 4.2e-7], "m3 kg-1")
            // Official WRF Registry diagnostic dataset names are lower-case.
            .field("d_ice", vec![0.002, f64::NAN, f64::NAN], "m")
            .field("rho_ice2", vec![f64::NAN, 300.0, f64::NAN], "kg m-3")
            .field("phi_ice2", vec![f64::NAN, 0.45, f64::NAN], "1")
            .field("v_ice2", vec![f64::NAN, 2.5, f64::NAN], "m s-1");
        provider.environment(271.65)
    }

    fn assert_close(actual: f64, expected: f64, tolerance: f64) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "{actual} != {expected} within {tolerance}"
        );
    }

    #[test]
    fn p3_50_and_51_normalize_actual_tuple_and_supply_environment() {
        for scheme_id in [50, 51] {
            let provider = p3_50_provider(scheme_id, 3, 1);
            let scene = read_property_scene(&provider, 0).unwrap();
            assert_eq!(scene.active_cell_indices(), &[1]);
            assert_eq!(scene.categories().len(), 1);
            assert_eq!(
                scene.categories()[0].category(),
                WrfPropertyCategory::P3(P3Category::Category1)
            );
            assert!(
                scene
                    .close_cell(0, OrientationDefinition::SchemeDefault)
                    .unwrap()
                    .is_none()
            );
            let cell = scene
                .close_cell(1, OrientationDefinition::SchemeDefault)
                .unwrap()
                .unwrap();
            assert_close(cell.environment().temperature_k(), 271.65, 2.0e-5);
            assert_close(
                cell.environment().air_density_kg_m3(),
                100_000.0 / (287.05 * 271.65),
                2.0e-6,
            );
            let ParticleState::P3(state) = cell.categories()[0].closed().record().state() else {
                panic!("expected P3 state")
            };
            assert_close(state.total_ice_mixing_ratio_kgkg(), 1.0e-4, 1.0e-10);
            assert_close(state.rime_mass_fraction(), 0.4, 1.0e-6);
            let qice_provenance = scene
                .source_fields()
                .iter()
                .find(|field| field.source_name() == "QICE")
                .unwrap();
            assert_eq!(qice_provenance.source_units(), "g kg-1");
            assert_eq!(
                qice_provenance.normalized_unit(),
                NormalizedUnit::KilogramsPerKilogram
            );
            assert!(provider.cache_clears.get() >= provider.reads.borrow().len() + 2);
        }
    }

    #[test]
    fn p3_52_retains_both_sparse_categories_and_sorted_union() {
        let scene = read_property_scene(&p3_52_provider(271.65), 0).unwrap();
        assert_eq!(scene.active_cell_indices(), &[1, 2, 3]);
        assert_eq!(scene.categories().len(), 2);
        assert_eq!(scene.categories()[0].active_cell_indices(), &[1, 3]);
        assert_eq!(scene.categories()[1].active_cell_indices(), &[2, 3]);
        assert!(
            scene
                .close_cell(0, OrientationDefinition::SchemeDefault)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            scene
                .close_cell(1, OrientationDefinition::SchemeDefault)
                .unwrap()
                .unwrap()
                .categories()
                .len(),
            1
        );
        let both = scene
            .close_cell(3, OrientationDefinition::SchemeDefault)
            .unwrap()
            .unwrap();
        assert_eq!(both.categories().len(), 2);
        assert_eq!(
            both.categories()[1].source_fields(),
            &["QICE2", "QNICE2", "QIR2", "QIB2"]
        );
    }

    #[test]
    fn p3_53_retains_qzi_and_recovers_native_sixth_moment() {
        let expected_m6 = 6.4e-17;
        let qnice: f64 = 1.0e6;
        let qzi = (expected_m6 * qnice).sqrt();
        let provider = TinyProvider::new(53, 1)
            .field("QICE", vec![1.0e-4], "kg kg-1")
            .field("QNICE", vec![qnice], "# kg-1")
            .field("QIR", vec![4.0e-5], "kg kg-1")
            .field("QIB", vec![1.0e-7], "m3 kg-1")
            .field("QZI", vec![qzi], "m(3) kg(-1)")
            .environment(271.65);
        let scene = read_property_scene(&provider, 0).unwrap();
        assert!(
            scene
                .required_field_signature()
                .field_names()
                .contains("QZI")
        );
        let cell = scene
            .close_cell(0, OrientationDefinition::SchemeDefault)
            .unwrap()
            .unwrap();
        let m6 = cell.categories()[0].closed().sixth_moment_m6().unwrap();
        assert_close(m6.value(), expected_m6, 2.0e-23);
        assert_eq!(m6.provenance().source_variables(), &["QZI", "QNICE"]);
    }

    #[test]
    fn ishmael_maps_three_real_tuples_and_diagnostics_field_by_field() {
        let scene = read_property_scene(&ishmael_provider(), 0).unwrap();
        assert_eq!(
            scene
                .categories()
                .iter()
                .map(SparsePropertyCategory::category)
                .collect::<Vec<_>>(),
            vec![
                WrfPropertyCategory::IshmaelPlanar,
                WrfPropertyCategory::IshmaelColumnar,
                WrfPropertyCategory::IshmaelAggregate,
            ]
        );
        for (cell_index, expected_category) in [
            (0, WrfPropertyCategory::IshmaelPlanar),
            (1, WrfPropertyCategory::IshmaelColumnar),
            (2, WrfPropertyCategory::IshmaelAggregate),
        ] {
            let cell = scene
                .close_cell(cell_index, OrientationDefinition::SchemeDefault)
                .unwrap()
                .unwrap();
            assert_eq!(cell.categories().len(), 1);
            assert_eq!(cell.categories()[0].category(), expected_category);
        }

        let planar = scene
            .close_cell(0, OrientationDefinition::SchemeDefault)
            .unwrap()
            .unwrap();
        assert_eq!(
            planar.categories()[0]
                .closed()
                .characteristic_diameter_m()
                .provenance()
                .source_variables(),
            &["d_ice"]
        );
        assert_eq!(
            planar.categories()[0]
                .closed()
                .effective_density_kg_m3()
                .provenance()
                .source_variables(),
            &["QICE", "QVOLI"]
        );

        let columnar = scene
            .close_cell(1, OrientationDefinition::SchemeDefault)
            .unwrap()
            .unwrap();
        let closed = columnar.categories()[0].closed();
        assert_eq!(
            closed
                .effective_density_kg_m3()
                .provenance()
                .source_variables(),
            &["rho_ice2"]
        );
        assert_eq!(
            closed
                .minor_to_major_axis_ratio()
                .provenance()
                .source_variables(),
            &["phi_ice2"]
        );
        assert_eq!(
            closed.fall_speed_m_s().provenance().source_variables(),
            &["v_ice2"]
        );
        let ParticleProvenance::Ishmael(provenance) = closed.record().provenance() else {
            panic!("expected ISHMAEL provenance")
        };
        assert_eq!(
            provenance
                .source_variables()
                .iter()
                .map(SourceVariable::name)
                .collect::<Vec<_>>(),
            vec![
                "QICE2", "QNICE2", "QVOLI2", "QAOLI2", "rho_ice2", "phi_ice2", "v_ice2",
            ]
        );

        let aggregate = scene
            .close_cell(2, OrientationDefinition::SchemeDefault)
            .unwrap()
            .unwrap();
        assert_eq!(
            aggregate.categories()[0]
                .closed()
                .characteristic_diameter_m()
                .provenance()
                .source_variables(),
            &["QICE3", "QNICE3", "QVOLI3"]
        );
        assert!(
            scene
                .required_field_signature()
                .field_names()
                .contains("d_ice")
        );
        assert!(!scene.categories().iter().any(|category| matches!(
            category.category().ishmael_category(),
            Some(IshmaelIceCategory::SmallIce | IshmaelIceCategory::Rimed)
        )));
    }

    #[test]
    fn zero_mass_tuple_skips_missing_properties_but_active_mass_errors() {
        let zero = TinyProvider::new(50, 2)
            .field("QICE", vec![0.0, 0.0], "kg kg-1")
            .environment(270.0);
        let scene = read_property_scene(&zero, 0).unwrap();
        assert!(scene.categories().is_empty());
        assert_eq!(
            scene.skipped_zero_mass_categories(),
            &[WrfPropertyCategory::P3(P3Category::Category1)]
        );
        assert!(scene.active_cell_indices().is_empty());

        let active_missing = TinyProvider::new(50, 1)
            .field("QICE", vec![1.0e-4], "kg kg-1")
            .environment(270.0);
        assert_eq!(
            read_property_scene(&active_missing, 0).unwrap_err(),
            WrfPropertyReadError::MissingRequiredField {
                category: WrfPropertyCategory::P3(P3Category::Category1),
                field: "QNICE",
            }
        );

        let active_missing_value = TinyProvider::new(50, 1)
            .field("QICE", vec![1.0e-4], "kg kg-1")
            .field("QNICE", vec![f64::NAN], "# kg-1")
            .field("QIR", vec![0.0], "kg kg-1")
            .field("QIB", vec![0.0], "m3 kg-1")
            .environment(270.0);
        assert_eq!(
            read_property_scene(&active_missing_value, 0).unwrap_err(),
            WrfPropertyReadError::MissingRequiredValue {
                category: WrfPropertyCategory::P3(P3Category::Category1),
                field: "QNICE",
                cell_index: 0,
            }
        );
    }

    #[test]
    fn sparse_memory_accounting_and_identity_exclude_dense_fields_and_private_path() {
        let provider = p3_50_provider(50, 1_024, 777);
        let scene = read_property_scene(&provider, 0).unwrap();
        let estimate = scene.memory_estimate();
        // One active-union index + one category index.
        assert_eq!(estimate.index_bytes, 2 * size_of::<u32>());
        // Dense temperature+density coverage plus four sparse P3 values.
        assert_eq!(estimate.value_bytes, (2 * 1_024 + 4) * size_of::<f32>());
        assert!(estimate.retained_bytes() < 1_024 * 6 * size_of::<f32>());
        assert_eq!(scene.active_cell_indices(), &[777]);
        assert_eq!(scene.identity().source_identity.0, "sha256:opaque-scene-id");
        let debug = format!("{scene:?}");
        assert!(!debug.contains("scientist"));
        assert!(!debug.contains("wrfout_d01"));
    }

    #[test]
    fn diagnostic_coexistence_components_do_not_accumulate_paired_rain_twice() {
        let scene = read_property_scene(&p3_52_provider(271.65), 0).unwrap();
        let cell = scene
            .close_cell(3, OrientationDefinition::SchemeDefault)
            .unwrap()
            .unwrap();
        let partition = cell
            .diagnose_coexistence(MixtureTopology::HomogeneousMixedPhase)
            .unwrap();
        let diagnosis = partition.diagnosis();
        assert!(diagnosis.paired_liquid_mass_kgkg() > 0.0);
        assert!(diagnosis.unused_rain_mass_kgkg() > 0.0);
        assert_close(
            partition.accounted_scattering_mass_kgkg(),
            diagnosis.input_total_mass_kgkg(),
            1.0e-12,
        );
        let wet_mass = partition
            .scattering_components()
            .into_iter()
            .filter_map(|component| match component {
                CoexistenceScatteringComponent::WetCategory(category) => {
                    Some(category.wet_total_mass_kgkg())
                }
                CoexistenceScatteringComponent::UnusedRain { .. } => None,
            })
            .sum::<f64>();
        assert_close(
            wet_mass,
            diagnosis.input_frozen_mass_kgkg() + diagnosis.paired_liquid_mass_kgkg(),
            1.0e-12,
        );
        assert_close(
            diagnosis.paired_liquid_mass_kgkg() + diagnosis.unused_rain_mass_kgkg(),
            diagnosis.input_rain_mass_kgkg(),
            1.0e-12,
        );
    }

    #[test]
    fn invalid_present_diagnostic_keeps_exact_resolved_field_in_error() {
        let provider =
            ishmael_provider().field("rho_ice2", vec![f64::NAN, -1.0, f64::NAN], "kg m-3");
        let scene = read_property_scene(&provider, 0).unwrap();
        let error = scene
            .close_cell(1, OrientationDefinition::SchemeDefault)
            .unwrap_err();
        assert!(matches!(
            error,
            WrfPropertyReadError::CategoryClosure {
                category: WrfPropertyCategory::IshmaelColumnar,
                source: ClosureError::OutOfRange {
                    field: "rho_ice2",
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn spatial_clear_to_echo_blends_raw_tuple_before_closure() {
        let scene = read_property_scene(&p3_50_provider(50, 2, 1), 0).unwrap();
        let clear = scene.raw_cell(0).unwrap();
        assert_eq!(clear.categories()[0].mixing_ratio_kgkg(), 0.0);
        let blended = blend_raw_property_cells(&[
            WeightedRawPropertyCell::new(&scene, 0, 0.5),
            WeightedRawPropertyCell::new(&scene, 1, 0.5),
        ])
        .unwrap();
        let RawPropertyCategory::P3(raw_p3) = &blended.categories()[0] else {
            panic!("expected raw P3 tuple")
        };
        assert_close(raw_p3.qice_kgkg, 5.0e-5, 1.0e-10);
        assert_close(raw_p3.qnice_per_kg, 5.0e5, 0.1);
        let closed =
            close_raw_property_cell(&blended, OrientationDefinition::SchemeDefault).unwrap();
        assert_eq!(closed.source_cell_index(), None);
        assert_eq!(closed.categories().len(), 1);
        assert_close(
            closed.categories()[0].closed().mixing_ratio_kgkg(),
            5.0e-5,
            1.0e-10,
        );
    }

    #[test]
    fn temporal_echo_birth_uses_complete_environment_and_raw_zero_endpoint() {
        let clear_provider = TinyProvider::new(50, 1)
            .field("QICE", vec![0.0], "kg kg-1")
            .field("QNICE", vec![0.0], "# kg-1")
            .field("QIR", vec![0.0], "kg kg-1")
            .field("QIB", vec![0.0], "m3 kg-1")
            .environment(268.0);
        let clear_scene = read_property_scene(&clear_provider, 0).unwrap();
        let echo_scene = read_property_scene(&p3_50_provider(50, 1, 0), 0).unwrap();
        let blended = blend_raw_property_cells(&[
            WeightedRawPropertyCell::new(&clear_scene, 0, 0.25),
            WeightedRawPropertyCell::new(&echo_scene, 0, 0.75),
        ])
        .unwrap();
        assert_close(blended.environment().temperature_k(), 270.7375, 3.0e-5);
        assert_close(blended.categories()[0].mixing_ratio_kgkg(), 7.5e-5, 1.0e-10);
        let closed =
            close_raw_property_cell(&blended, OrientationDefinition::SchemeDefault).unwrap();
        assert_close(
            closed.categories()[0].closed().mixing_ratio_kgkg(),
            7.5e-5,
            1.0e-10,
        );
    }

    #[test]
    fn raw_blend_rejects_required_field_signature_mismatch() {
        let left = read_property_scene(&p3_50_provider(50, 1, 0), 0).unwrap();
        let right_provider = p3_50_provider(50, 1, 0)
            .field("QRAIN", vec![1.0e-4], "kg kg-1")
            .field("QNRAIN", vec![1.0e6], "# kg-1");
        let right = read_property_scene(&right_provider, 0).unwrap();
        assert_eq!(
            blend_raw_property_cells(&[
                WeightedRawPropertyCell::new(&left, 0, 0.5),
                WeightedRawPropertyCell::new(&right, 0, 0.5),
            ])
            .unwrap_err(),
            RawPropertyBlendError::FieldSignatureMismatch { sample_index: 1 }
        );
    }

    #[test]
    fn missing_scheme_rain_surfaces_typed_coexistence_unavailable() {
        let scene = read_property_scene(&p3_50_provider(50, 1, 0), 0).unwrap();
        let closed = scene
            .close_cell(0, OrientationDefinition::SchemeDefault)
            .unwrap()
            .unwrap();
        assert!(matches!(
            closed.rain(),
            ClosedRainState::Unavailable(RainUnavailableReason::MissingMassField {
                field: "QRAIN"
            })
        ));
        assert!(matches!(
            closed.diagnose_coexistence(MixtureTopology::HomogeneousMixedPhase),
            Err(CoexistenceUnavailable::RainUnavailable(
                RainUnavailableReason::MissingMassField { field: "QRAIN" }
            ))
        ));
    }

    #[test]
    fn source_signature_is_ready_for_temporal_compatibility_without_lut_claims() {
        let p3_50 = read_property_scene(&p3_50_provider(50, 1, 0), 0).unwrap();
        let mut p3_53_provider = p3_50_provider(53, 1, 0);
        p3_53_provider
            .fields
            .insert("QZI", RawPropertyField::new(vec![8.0e-6], "m3 kg-1"));
        let p3_53 = read_property_scene(&p3_53_provider, 0).unwrap();
        assert!(
            !p3_50
                .required_field_signature()
                .field_names()
                .contains("QZI")
        );
        assert!(
            p3_53
                .required_field_signature()
                .field_names()
                .contains("QZI")
        );
        let temporal = p3_53.temporal_signature("future property LUT (not evaluated)");
        assert_eq!(temporal.microphysics_scheme_id, Some(53));
        assert_eq!(
            temporal.required_raw_fields,
            p3_53.required_field_signature().field_names()
        );
    }
}
