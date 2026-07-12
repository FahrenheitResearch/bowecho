//! Fail-closed property-aware T-matrix cache for one WRF model instant.
//!
//! The cache retains only sparse source-cell indices and nine additive f32
//! components at every validated radar-elevation LUT node.  Query-time
//! elevation interpolation happens component by component; nonlinear radar
//! products are derived only after interpolation.  These research tables are
//! never substituted with the bulk Rayleigh operator when a lookup fails.

use std::mem::size_of;

use radar_scattering::{
    AdditiveScattering, AxisKind, ClosedParticleCategory, ConventionalHydrometeor,
    DIAGNOSTIC_COEXISTENCE_COLD_K, DIAGNOSTIC_COEXISTENCE_WARM_K, EvaluationError,
    FallMomentPolicy, IshmaelIceCategory, MixtureTopology, OrientationDefinition, OutputError,
    ParticleState, PolarAccumulatorQuantities, RadarViewApplicability, RadarViewGeometry,
    ResearchTMatrixLut, Sha256Digest, SpheroidConvention, TMatrixEvaluationRequest,
    TMatrixMaterial, TMatrixOdfConvention, TMatrixParticleCategory, TMatrixPopulationRole,
};
use rayon::prelude::*;
use thiserror::Error;

use crate::wrf_property_reader::{
    ClosedRainState, CoexistenceScatteringComponent, CoexistenceUnavailable, PropertySceneIdentity,
    RainUnavailableReason, RequiredFieldSignature, SourceFieldProvenance, WrfPropertyCategory,
    WrfPropertyReadError, WrfPropertyScene,
};
use crate::wrf_temporal::ScenePropertySignature;

pub const PROPERTY_TMATRIX_FREQUENCY_HZ: f64 = 2_800_000_000.0;
pub const PROPERTY_TMATRIX_MIN_ELEVATION_DEG: f64 = -0.5;
pub const PROPERTY_TMATRIX_MAX_ELEVATION_DEG: f64 = 20.0;

const DRY_OBLATE_TABLE_ID: &str =
    "property-p3-ishmael-dry-oblate-sband-pytmatrix-0.3.3-unvalidated-v1";
const DRY_PROLATE_TABLE_ID: &str =
    "property-p3-ishmael-dry-prolate-sband-pytmatrix-0.3.3-unvalidated-v1";
const WET_OBLATE_TABLE_ID: &str =
    "property-p3-ishmael-wet-oblate-sband-pytmatrix-0.3.3-unvalidated-v1";
const WET_PROLATE_TABLE_ID: &str =
    "property-p3-ishmael-wet-prolate-sband-pytmatrix-0.3.3-unvalidated-v1";
const RAIN_TABLE_ID: &str = "property-rain-sband-pytmatrix-0.3.3-unvalidated-v1";

const DRY_AXIS_KINDS: &[AxisKind] = &[
    AxisKind::EquivolumeDiameter,
    AxisKind::Temperature,
    AxisKind::BulkDensity,
    AxisKind::MinorToMajorAxisRatio,
    AxisKind::Frequency,
    AxisKind::RadarElevation,
];
const WET_AXIS_KINDS: &[AxisKind] = &[
    AxisKind::EquivolumeDiameter,
    AxisKind::Temperature,
    AxisKind::CondensedVolumeFraction,
    AxisKind::LiquidMassFraction,
    AxisKind::MinorToMajorAxisRatio,
    AxisKind::Frequency,
    AxisKind::RadarElevation,
];
const RAIN_AXIS_KINDS: &[AxisKind] = &[
    AxisKind::EquivolumeDiameter,
    AxisKind::Temperature,
    AxisKind::MinorToMajorAxisRatio,
    AxisKind::Frequency,
    AxisKind::RadarElevation,
];

/// Exact role of each reference in the five-table scene bundle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WrfTMatrixTableRole {
    DryOblate,
    DryProlate,
    WetOblate,
    WetProlate,
    RainStandaloneAndResidual,
}

impl WrfTMatrixTableRole {
    const fn expected_id(self) -> &'static str {
        match self {
            Self::DryOblate => DRY_OBLATE_TABLE_ID,
            Self::DryProlate => DRY_PROLATE_TABLE_ID,
            Self::WetOblate => WET_OBLATE_TABLE_ID,
            Self::WetProlate => WET_PROLATE_TABLE_ID,
            Self::RainStandaloneAndResidual => RAIN_TABLE_ID,
        }
    }
}

impl std::fmt::Display for WrfTMatrixTableRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::DryOblate => "dry oblate",
            Self::DryProlate => "dry prolate",
            Self::WetOblate => "wet oblate",
            Self::WetProlate => "wet prolate",
            Self::RainStandaloneAndResidual => "standalone/residual rain",
        })
    }
}

/// Borrowed, typed inputs required to build a property-scattering scene.
#[derive(Clone, Copy)]
pub struct WrfTMatrixLutBundle<'a> {
    pub dry_oblate: &'a ResearchTMatrixLut,
    pub dry_prolate: &'a ResearchTMatrixLut,
    pub wet_oblate: &'a ResearchTMatrixLut,
    pub wet_prolate: &'a ResearchTMatrixLut,
    pub rain_standalone_and_residual: &'a ResearchTMatrixLut,
}

impl<'a> WrfTMatrixLutBundle<'a> {
    #[must_use]
    pub const fn new(
        dry_oblate: &'a ResearchTMatrixLut,
        dry_prolate: &'a ResearchTMatrixLut,
        wet_oblate: &'a ResearchTMatrixLut,
        wet_prolate: &'a ResearchTMatrixLut,
        rain_standalone_and_residual: &'a ResearchTMatrixLut,
    ) -> Self {
        Self {
            dry_oblate,
            dry_prolate,
            wet_oblate,
            wet_prolate,
            rain_standalone_and_residual,
        }
    }

    fn entries(self) -> [(WrfTMatrixTableRole, &'a ResearchTMatrixLut); 5] {
        [
            (WrfTMatrixTableRole::DryOblate, self.dry_oblate),
            (WrfTMatrixTableRole::DryProlate, self.dry_prolate),
            (WrfTMatrixTableRole::WetOblate, self.wet_oblate),
            (WrfTMatrixTableRole::WetProlate, self.wet_prolate),
            (
                WrfTMatrixTableRole::RainStandaloneAndResidual,
                self.rain_standalone_and_residual,
            ),
        ]
    }

    fn dry(self, spheroid: SpheroidConvention) -> &'a ResearchTMatrixLut {
        match spheroid {
            SpheroidConvention::OblateMinorVertical => self.dry_oblate,
            SpheroidConvention::ProlateMajorVertical => self.dry_prolate,
        }
    }

    fn wet(self, spheroid: SpheroidConvention) -> &'a ResearchTMatrixLut {
        match spheroid {
            SpheroidConvention::OblateMinorVertical => self.wet_oblate,
            SpheroidConvention::ProlateMajorVertical => self.wet_prolate,
        }
    }
}

/// Whether rain and diagnosed mixed-phase coexistence are required.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WrfTMatrixRainMode {
    /// Require the WRF rain mass and number fields, evaluate standalone rain,
    /// and diagnose wet frozen/rain coexistence inside the exact envelope.
    #[default]
    FullProperty,
    /// Deliberately omit all rain and wet-coexistence scattering. Frozen
    /// categories remain dry. This is explicit, never an automatic fallback.
    FrozenOnly,
}

/// Additive-population counts, independent of the number of elevation nodes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WrfTMatrixAuditCounts {
    pub source_cells: u64,
    pub dry_frozen_populations: u64,
    pub wet_frozen_populations: u64,
    pub residual_rain_populations: u64,
    pub standalone_rain_populations: u64,
}

impl WrfTMatrixAuditCounts {
    fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            source_cells: self.source_cells.checked_add(other.source_cells)?,
            dry_frozen_populations: self
                .dry_frozen_populations
                .checked_add(other.dry_frozen_populations)?,
            wet_frozen_populations: self
                .wet_frozen_populations
                .checked_add(other.wet_frozen_populations)?,
            residual_rain_populations: self
                .residual_rain_populations
                .checked_add(other.residual_rain_populations)?,
            standalone_rain_populations: self
                .standalone_rain_populations
                .checked_add(other.standalone_rain_populations)?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WrfTMatrixTableAudit {
    pub role: WrfTMatrixTableRole,
    pub table_id: &'static str,
    pub file_sha256: Sha256Digest,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WrfTMatrixSceneProvenance {
    pub status: &'static str,
    pub frequency_hz: f64,
    pub orientation: OrientationDefinition,
    pub fall_moment_policy: WrfTMatrixFallMomentAudit,
    pub rain_mode: WrfTMatrixRainMode,
    pub tables: [WrfTMatrixTableAudit; 5],
    pub counts: WrfTMatrixAuditCounts,
}

/// Runtime Doppler fall-moment policies retained with the scene audit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WrfTMatrixFallMomentAudit {
    /// Dry frozen, standalone rain, and residual rain.
    pub closed_category: FallMomentPolicy,
    /// Wet frozen populations diagnosed by DiagnosticCoexistenceV1.
    pub diagnostic_wet_category: FallMomentPolicy,
}

/// Logical retained-memory estimate; allocator slack is intentionally absent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WrfTMatrixSceneMemoryEstimate {
    pub structure_bytes: usize,
    pub source_index_bytes: usize,
    pub dense_row_lookup_bytes: usize,
    pub elevation_axis_bytes: usize,
    pub additive_component_bytes: usize,
    pub source_identity_bytes: usize,
    pub required_field_contract_bytes: usize,
    pub source_field_provenance_bytes: usize,
    pub provenance_text_bytes: usize,
}

impl WrfTMatrixSceneMemoryEstimate {
    #[must_use]
    pub const fn retained_bytes(self) -> usize {
        self.structure_bytes
            .saturating_add(self.source_index_bytes)
            .saturating_add(self.dense_row_lookup_bytes)
            .saturating_add(self.elevation_axis_bytes)
            .saturating_add(self.additive_component_bytes)
            .saturating_add(self.source_identity_bytes)
            .saturating_add(self.required_field_contract_bytes)
            .saturating_add(self.source_field_provenance_bytes)
            .saturating_add(self.provenance_text_bytes)
    }
}

/// Sparse precomputed additive scattering for one WRF property scene.
#[derive(Clone, Debug, PartialEq)]
pub struct WrfTMatrixScene {
    source_identity: PropertySceneIdentity,
    microphysics_scheme_id: i32,
    required_field_signature: RequiredFieldSignature,
    source_fields: Vec<SourceFieldProvenance>,
    source_cell_count: usize,
    source_cell_indices: Vec<u32>,
    /// O(1) full-cell to compact-row lookup; u32::MAX means clear/omitted.
    full_cell_to_compact_row: Vec<u32>,
    radar_elevations_deg: Vec<f64>,
    /// Cell-major, then elevation-major, then canonical nine components.
    additive_components: Vec<f32>,
    provenance: WrfTMatrixSceneProvenance,
}

impl WrfTMatrixScene {
    /// Build the complete property scene. Missing rain inputs are errors.
    pub fn build(
        source: &WrfPropertyScene,
        tables: WrfTMatrixLutBundle<'_>,
    ) -> Result<Self, WrfTMatrixSceneBuildError> {
        Self::build_with_rain_mode(source, tables, WrfTMatrixRainMode::FullProperty)
    }

    pub fn build_with_rain_mode(
        source: &WrfPropertyScene,
        tables: WrfTMatrixLutBundle<'_>,
        rain_mode: WrfTMatrixRainMode,
    ) -> Result<Self, WrfTMatrixSceneBuildError> {
        let validated = validate_bundle(tables)?;
        if source.cell_count() > u32::MAX as usize {
            return Err(WrfTMatrixSceneBuildError::GridTooLarge {
                cell_count: source.cell_count(),
            });
        }
        validate_sparse_indices(source)?;
        let elevations = validated.radar_elevations_deg;
        let rows = source
            .active_cell_indices()
            .par_iter()
            .map(|&cell_index| {
                build_cell(
                    source,
                    usize::try_from(cell_index).expect("u32 always fits usize"),
                    tables,
                    elevations,
                    rain_mode,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;

        let components_per_cell = elevations
            .len()
            .checked_mul(AdditiveScattering::COMPONENT_COUNT)
            .ok_or(WrfTMatrixSceneBuildError::SizeOverflow)?;
        let retained_rows = rows.iter().filter(|row| !row.components.is_empty()).count();
        let total_components = retained_rows
            .checked_mul(components_per_cell)
            .ok_or(WrfTMatrixSceneBuildError::SizeOverflow)?;
        let mut additive_components = Vec::with_capacity(total_components);
        let mut source_cell_indices = Vec::with_capacity(retained_rows);
        let mut counts = WrfTMatrixAuditCounts::default();
        for row in rows {
            if row.components.is_empty() {
                continue;
            }
            if row.components.len() != components_per_cell {
                return Err(WrfTMatrixSceneBuildError::InternalRowLength {
                    cell_index: row.cell_index,
                    expected: components_per_cell,
                    actual: row.components.len(),
                });
            }
            source_cell_indices.push(
                u32::try_from(row.cell_index)
                    .map_err(|_| WrfTMatrixSceneBuildError::SizeOverflow)?,
            );
            additive_components.extend(row.components);
            counts = counts
                .checked_add(row.counts)
                .ok_or(WrfTMatrixSceneBuildError::AuditCountOverflow)?;
        }
        let mut full_cell_to_compact_row = vec![u32::MAX; source.cell_count()];
        for (compact_row, &source_cell) in source_cell_indices.iter().enumerate() {
            let compact_row =
                u32::try_from(compact_row).map_err(|_| WrfTMatrixSceneBuildError::SizeOverflow)?;
            if compact_row == u32::MAX {
                return Err(WrfTMatrixSceneBuildError::SizeOverflow);
            }
            full_cell_to_compact_row[source_cell as usize] = compact_row;
        }

        let table_audits = tables.entries().map(|(role, table)| WrfTMatrixTableAudit {
            role,
            table_id: role.expected_id(),
            file_sha256: table.file_sha256(),
        });
        Ok(Self {
            source_identity: source.identity().clone(),
            microphysics_scheme_id: source.microphysics_scheme_id(),
            required_field_signature: source.required_field_signature().clone(),
            source_fields: source.source_fields().to_vec(),
            source_cell_count: source.cell_count(),
            source_cell_indices,
            full_cell_to_compact_row,
            radar_elevations_deg: elevations.to_vec(),
            additive_components,
            provenance: WrfTMatrixSceneProvenance {
                status: "research_only_unvalidated",
                frequency_hz: PROPERTY_TMATRIX_FREQUENCY_HZ,
                orientation: OrientationDefinition::Gaussian20Research,
                fall_moment_policy: WrfTMatrixFallMomentAudit {
                    closed_category:
                        FallMomentPolicy::ClosedCategoryPositiveDownZeroWithinCategoryVariance,
                    diagnostic_wet_category:
                        FallMomentPolicy::DiagnosticWetCategoryPositiveDownZeroWithinCategoryVariance,
                },
                rain_mode,
                tables: table_audits,
                counts,
            },
        })
    }

    #[must_use]
    pub const fn source_identity(&self) -> &PropertySceneIdentity {
        &self.source_identity
    }

    #[must_use]
    pub const fn microphysics_scheme_id(&self) -> i32 {
        self.microphysics_scheme_id
    }

    #[must_use]
    pub const fn required_field_signature(&self) -> &RequiredFieldSignature {
        &self.required_field_signature
    }

    #[must_use]
    pub fn source_fields(&self) -> &[SourceFieldProvenance] {
        &self.source_fields
    }

    /// Temporal compatibility retained after the raw property arrays have
    /// been dropped. The caller supplies its actual renderer/LUT label.
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

    #[must_use]
    pub const fn source_cell_count(&self) -> usize {
        self.source_cell_count
    }

    #[must_use]
    pub fn active_cell_indices(&self) -> &[u32] {
        &self.source_cell_indices
    }

    #[must_use]
    pub fn radar_elevations_deg(&self) -> &[f64] {
        &self.radar_elevations_deg
    }

    #[must_use]
    pub const fn provenance(&self) -> &WrfTMatrixSceneProvenance {
        &self.provenance
    }

    /// Interpolate only additive quantities at an arbitrary in-range beam
    /// elevation. A clear/non-active source cell returns `Ok(None)`.
    pub fn additive_at(
        &self,
        full_cell_index: usize,
        beam_elevation_deg: f64,
    ) -> Result<Option<AdditiveScattering>, WrfTMatrixSceneQueryError> {
        if full_cell_index >= self.source_cell_count {
            return Err(WrfTMatrixSceneQueryError::CellOutOfRange {
                cell_index: full_cell_index,
                cell_count: self.source_cell_count,
            });
        }
        let compact_cell = *self
            .full_cell_to_compact_row
            .get(full_cell_index)
            .ok_or(WrfTMatrixSceneQueryError::CorruptStorage)?;
        if compact_cell == u32::MAX {
            return Ok(None);
        }
        let compact_cell = compact_cell as usize;
        let bracket = elevation_bracket(&self.radar_elevations_deg, beam_elevation_deg)?;
        let lower = self.component_row(compact_cell, bracket.lower)?;
        if bracket.lower == bracket.upper {
            return decode_components(lower).map(Some);
        }
        let upper = self.component_row(compact_cell, bracket.upper)?;
        let mut interpolated = [0.0; AdditiveScattering::COMPONENT_COUNT];
        for component in 0..AdditiveScattering::COMPONENT_COUNT {
            interpolated[component] = f64::from(lower[component])
                + bracket.fraction * (f64::from(upper[component]) - f64::from(lower[component]));
        }
        AdditiveScattering::from_components(interpolated)
            .map(Some)
            .map_err(WrfTMatrixSceneQueryError::Output)
    }

    pub fn polar_at(
        &self,
        full_cell_index: usize,
        beam_elevation_deg: f64,
    ) -> Result<Option<PolarAccumulatorQuantities>, WrfTMatrixSceneQueryError> {
        self.additive_at(full_cell_index, beam_elevation_deg)?
            .map(AdditiveScattering::to_polar_accumulator_quantities)
            .transpose()
            .map_err(WrfTMatrixSceneQueryError::Output)
    }

    #[must_use]
    pub fn memory_estimate(&self) -> WrfTMatrixSceneMemoryEstimate {
        let required_field_contract_bytes = self.required_field_signature.fields.len()
            * size_of::<crate::wrf_property_reader::RequiredFieldContract>();
        let source_field_provenance_bytes =
            self.source_fields.len() * size_of::<SourceFieldProvenance>();
        let provenance_text_bytes = self
            .source_fields
            .iter()
            .map(|field| field.source_units().len())
            .sum();
        WrfTMatrixSceneMemoryEstimate {
            structure_bytes: size_of::<Self>(),
            source_index_bytes: self.source_cell_indices.len() * size_of::<u32>(),
            dense_row_lookup_bytes: self.full_cell_to_compact_row.len() * size_of::<u32>(),
            elevation_axis_bytes: self.radar_elevations_deg.len() * size_of::<f64>(),
            additive_component_bytes: self.additive_components.len() * size_of::<f32>(),
            source_identity_bytes: self.source_identity.source_identity.0.len(),
            required_field_contract_bytes,
            source_field_provenance_bytes,
            provenance_text_bytes,
        }
    }

    fn component_row(
        &self,
        compact_cell: usize,
        elevation_index: usize,
    ) -> Result<&[f32], WrfTMatrixSceneQueryError> {
        let row = compact_cell
            .checked_mul(self.radar_elevations_deg.len())
            .and_then(|value| value.checked_add(elevation_index))
            .and_then(|value| value.checked_mul(AdditiveScattering::COMPONENT_COUNT))
            .ok_or(WrfTMatrixSceneQueryError::CorruptStorage)?;
        let end = row
            .checked_add(AdditiveScattering::COMPONENT_COUNT)
            .ok_or(WrfTMatrixSceneQueryError::CorruptStorage)?;
        self.additive_components
            .get(row..end)
            .ok_or(WrfTMatrixSceneQueryError::CorruptStorage)
    }
}

struct ValidatedBundle<'a> {
    radar_elevations_deg: &'a [f64],
}

#[derive(Debug)]
struct BuiltCell {
    cell_index: usize,
    components: Vec<f32>,
    counts: WrfTMatrixAuditCounts,
}

fn build_cell(
    source: &WrfPropertyScene,
    cell_index: usize,
    tables: WrfTMatrixLutBundle<'_>,
    elevations: &[f64],
    rain_mode: WrfTMatrixRainMode,
) -> Result<BuiltCell, WrfTMatrixSceneBuildError> {
    let closed = source
        .close_cell(cell_index, OrientationDefinition::Gaussian20Research)
        .map_err(|source| WrfTMatrixSceneBuildError::SourceCell { cell_index, source })?;
    let Some(closed) = closed else {
        return if rain_mode == WrfTMatrixRainMode::FrozenOnly {
            Ok(BuiltCell {
                cell_index,
                components: Vec::new(),
                counts: WrfTMatrixAuditCounts::default(),
            })
        } else {
            Err(WrfTMatrixSceneBuildError::UnexpectedClearActiveCell { cell_index })
        };
    };

    let rain = match closed.rain() {
        ClosedRainState::Clear => None,
        ClosedRainState::Closed(rain) => Some(rain.as_ref()),
        ClosedRainState::Unavailable(reason) => match rain_mode {
            WrfTMatrixRainMode::FullProperty => {
                return Err(WrfTMatrixSceneBuildError::RainUnavailable {
                    cell_index,
                    reason: reason.clone(),
                });
            }
            WrfTMatrixRainMode::FrozenOnly => None,
        },
    };
    let has_frozen = !closed.categories().is_empty();
    if rain_mode == WrfTMatrixRainMode::FrozenOnly && !has_frozen {
        return Ok(BuiltCell {
            cell_index,
            components: Vec::new(),
            counts: WrfTMatrixAuditCounts::default(),
        });
    }
    let coexistence = if should_diagnose_wet_coexistence(
        rain_mode,
        has_frozen,
        rain.is_some(),
        closed.environment().temperature_k(),
    ) {
        Some(
            closed
                .diagnose_coexistence(MixtureTopology::HomogeneousMixedPhase)
                .map_err(|source| WrfTMatrixSceneBuildError::Coexistence { cell_index, source })?,
        )
    } else {
        None
    };

    let counts = if let Some(partition) = &coexistence {
        WrfTMatrixAuditCounts {
            source_cells: 1,
            wet_frozen_populations: usize_to_u64(partition.diagnosis().wet_categories().len())?,
            residual_rain_populations: u64::from(
                partition.diagnosis().unused_rain_mass_kgkg() > 0.0,
            ),
            ..WrfTMatrixAuditCounts::default()
        }
    } else {
        WrfTMatrixAuditCounts {
            source_cells: 1,
            dry_frozen_populations: usize_to_u64(closed.categories().len())?,
            standalone_rain_populations: u64::from(
                rain_mode == WrfTMatrixRainMode::FullProperty && rain.is_some(),
            ),
            ..WrfTMatrixAuditCounts::default()
        }
    };

    let component_capacity = elevations
        .len()
        .checked_mul(AdditiveScattering::COMPONENT_COUNT)
        .ok_or(WrfTMatrixSceneBuildError::SizeOverflow)?;
    let mut compact = Vec::with_capacity(component_capacity);
    for &elevation_deg in elevations {
        let mut additive = AdditiveScattering::default();
        if let Some(partition) = &coexistence {
            for component in partition.scattering_components() {
                match component {
                    CoexistenceScatteringComponent::WetCategory(wet) => {
                        let spheroid = spheroid_for_particle(wet.source_category())?;
                        let request = evaluation_request(elevation_deg, spheroid)?;
                        let contribution = tables
                            .wet(spheroid)
                            .evaluate_wet_category(wet, request)
                            .map_err(|source| WrfTMatrixSceneBuildError::Evaluation {
                                cell_index,
                                elevation_deg,
                                contribution: WrfTMatrixContribution::WetFrozen,
                                source,
                            })?;
                        verify_fall_moment_policy(
                            contribution.fall_moments(),
                            FallMomentPolicy::DiagnosticWetCategoryPositiveDownZeroWithinCategoryVariance,
                            WrfTMatrixContribution::WetFrozen,
                        )?;
                        additive =
                            additive
                                .checked_add(contribution.additive())
                                .map_err(|source| WrfTMatrixSceneBuildError::Accumulation {
                                    cell_index,
                                    elevation_deg,
                                    contribution: WrfTMatrixContribution::WetFrozen,
                                    source,
                                })?;
                    }
                    CoexistenceScatteringComponent::UnusedRain {
                        source: rain_source,
                        mixing_ratio_kgkg,
                    } => {
                        let request = evaluation_request(
                            elevation_deg,
                            SpheroidConvention::OblateMinorVertical,
                        )?;
                        let contribution = tables
                            .rain_standalone_and_residual
                            .evaluate_unused_rain(rain_source, mixing_ratio_kgkg, request)
                            .map_err(|source| WrfTMatrixSceneBuildError::Evaluation {
                                cell_index,
                                elevation_deg,
                                contribution: WrfTMatrixContribution::ResidualRain,
                                source,
                            })?;
                        verify_fall_moment_policy(
                            contribution.fall_moments(),
                            FallMomentPolicy::ClosedCategoryPositiveDownZeroWithinCategoryVariance,
                            WrfTMatrixContribution::ResidualRain,
                        )?;
                        additive =
                            additive
                                .checked_add(contribution.additive())
                                .map_err(|source| WrfTMatrixSceneBuildError::Accumulation {
                                    cell_index,
                                    elevation_deg,
                                    contribution: WrfTMatrixContribution::ResidualRain,
                                    source,
                                })?;
                    }
                }
            }
        } else {
            for category in closed.categories() {
                let spheroid = spheroid_for_category(category.category());
                let request = evaluation_request(elevation_deg, spheroid)?;
                let contribution = tables
                    .dry(spheroid)
                    .evaluate(category.closed(), request)
                    .map_err(|source| WrfTMatrixSceneBuildError::Evaluation {
                        cell_index,
                        elevation_deg,
                        contribution: WrfTMatrixContribution::DryFrozen,
                        source,
                    })?;
                additive = additive.checked_add(contribution).map_err(|source| {
                    WrfTMatrixSceneBuildError::Accumulation {
                        cell_index,
                        elevation_deg,
                        contribution: WrfTMatrixContribution::DryFrozen,
                        source,
                    }
                })?;
            }
            if rain_mode == WrfTMatrixRainMode::FullProperty
                && let Some(rain) = rain
            {
                let request =
                    evaluation_request(elevation_deg, SpheroidConvention::OblateMinorVertical)?;
                let contribution = tables
                    .rain_standalone_and_residual
                    .evaluate(rain, request)
                    .map_err(|source| WrfTMatrixSceneBuildError::Evaluation {
                        cell_index,
                        elevation_deg,
                        contribution: WrfTMatrixContribution::StandaloneRain,
                        source,
                    })?;
                additive = additive.checked_add(contribution).map_err(|source| {
                    WrfTMatrixSceneBuildError::Accumulation {
                        cell_index,
                        elevation_deg,
                        contribution: WrfTMatrixContribution::StandaloneRain,
                        source,
                    }
                })?;
            }
        }
        compact.extend(compact_components(additive).map_err(|source| {
            WrfTMatrixSceneBuildError::Compact {
                cell_index,
                elevation_deg,
                source,
            }
        })?);
    }
    Ok(BuiltCell {
        cell_index,
        components: compact,
        counts,
    })
}

fn should_diagnose_wet_coexistence(
    rain_mode: WrfTMatrixRainMode,
    has_frozen: bool,
    has_rain: bool,
    temperature_k: f64,
) -> bool {
    // At the exact cold boundary DiagnosticCoexistenceV1 pairs zero liquid.
    // That is dry frozen plus full standalone rain, not a wet-table query.
    rain_mode == WrfTMatrixRainMode::FullProperty
        && has_frozen
        && has_rain
        && temperature_k > DIAGNOSTIC_COEXISTENCE_COLD_K
        && temperature_k <= DIAGNOSTIC_COEXISTENCE_WARM_K
}

fn verify_fall_moment_policy(
    actual: FallMomentPolicy,
    expected: FallMomentPolicy,
    contribution: WrfTMatrixContribution,
) -> Result<(), WrfTMatrixSceneBuildError> {
    if actual == expected {
        Ok(())
    } else {
        Err(WrfTMatrixSceneBuildError::FallMomentPolicy {
            contribution,
            expected,
            actual,
        })
    }
}

fn evaluation_request(
    elevation_deg: f64,
    spheroid: SpheroidConvention,
) -> Result<TMatrixEvaluationRequest, WrfTMatrixSceneBuildError> {
    let view = RadarViewGeometry::new(elevation_deg)
        .map_err(WrfTMatrixSceneBuildError::EvaluationRequest)?;
    TMatrixEvaluationRequest::new(PROPERTY_TMATRIX_FREQUENCY_HZ, spheroid, view)
        .map_err(WrfTMatrixSceneBuildError::EvaluationRequest)
}

fn spheroid_for_category(category: WrfPropertyCategory) -> SpheroidConvention {
    match category {
        WrfPropertyCategory::IshmaelColumnar => SpheroidConvention::ProlateMajorVertical,
        WrfPropertyCategory::P3(_)
        | WrfPropertyCategory::IshmaelPlanar
        | WrfPropertyCategory::IshmaelAggregate => SpheroidConvention::OblateMinorVertical,
    }
}

fn spheroid_for_particle(
    particle: &ClosedParticleCategory,
) -> Result<SpheroidConvention, WrfTMatrixSceneBuildError> {
    match particle.record().state() {
        ParticleState::P3(_) => Ok(SpheroidConvention::OblateMinorVertical),
        ParticleState::Ishmael(state) if state.category() == IshmaelIceCategory::Columnar => {
            Ok(SpheroidConvention::ProlateMajorVertical)
        }
        ParticleState::Ishmael(_) => Ok(SpheroidConvention::OblateMinorVertical),
        ParticleState::Conventional(_) => Err(WrfTMatrixSceneBuildError::WetSourceNotFrozen),
    }
}

fn compact_components(additive: AdditiveScattering) -> Result<[f32; 9], CompactScatteringError> {
    let values = additive.components();
    let mut compact = [0.0_f32; 9];
    for (index, value) in values.into_iter().enumerate() {
        if value < -(f32::MAX as f64) || value > f32::MAX as f64 {
            return Err(CompactScatteringError::OutsideF32 { index, value });
        }
        let converted = value as f32;
        if !converted.is_finite() {
            return Err(CompactScatteringError::OutsideF32 { index, value });
        }
        compact[index] = converted;
    }
    // Quantization must still satisfy covariance and fall-moment invariants;
    // never repair an invalid compact tuple by saturation or clamping.
    decode_components(&compact).map_err(|error| match error {
        WrfTMatrixSceneQueryError::Output(source) => CompactScatteringError::RoundTrip(source),
        _ => unreachable!("decoding a fixed nine-component array cannot be a storage error"),
    })?;
    Ok(compact)
}

fn decode_components(values: &[f32]) -> Result<AdditiveScattering, WrfTMatrixSceneQueryError> {
    if values.len() != AdditiveScattering::COMPONENT_COUNT {
        return Err(WrfTMatrixSceneQueryError::CorruptStorage);
    }
    let mut decoded = [0.0; AdditiveScattering::COMPONENT_COUNT];
    for (target, source) in decoded.iter_mut().zip(values) {
        *target = f64::from(*source);
    }
    AdditiveScattering::from_components(decoded).map_err(WrfTMatrixSceneQueryError::Output)
}

fn validate_bundle(
    tables: WrfTMatrixLutBundle<'_>,
) -> Result<ValidatedBundle<'_>, WrfTMatrixBundleError> {
    for (role, table) in tables.entries() {
        let expected_category = if role == WrfTMatrixTableRole::RainStandaloneAndResidual {
            TMatrixParticleCategory::Conventional(ConventionalHydrometeor::Rain)
        } else {
            TMatrixParticleCategory::PropertyAwareFrozenCharacteristicParticle
        };
        let expected_population = match role {
            WrfTMatrixTableRole::DryOblate | WrfTMatrixTableRole::DryProlate => {
                TMatrixPopulationRole::PropertyAwareDryCharacteristicParticle
            }
            WrfTMatrixTableRole::WetOblate | WrfTMatrixTableRole::WetProlate => {
                TMatrixPopulationRole::PropertyAwareWetCharacteristicParticle
            }
            WrfTMatrixTableRole::RainStandaloneAndResidual => {
                TMatrixPopulationRole::ConventionalRainStandaloneAndResidual
            }
        };
        let expected_spheroid = match role {
            WrfTMatrixTableRole::DryProlate | WrfTMatrixTableRole::WetProlate => {
                SpheroidConvention::ProlateMajorVertical
            }
            WrfTMatrixTableRole::DryOblate
            | WrfTMatrixTableRole::WetOblate
            | WrfTMatrixTableRole::RainStandaloneAndResidual => {
                SpheroidConvention::OblateMinorVertical
            }
        };
        let descriptor = table.descriptor();
        if descriptor.table_id() != role.expected_id() {
            return Err(WrfTMatrixBundleError::TableId {
                role,
                expected: role.expected_id(),
                actual: descriptor.table_id().to_owned(),
            });
        }
        if descriptor.category() != expected_category {
            return Err(WrfTMatrixBundleError::Category {
                role,
                expected: expected_category,
                actual: descriptor.category(),
            });
        }
        if descriptor.population_role() != expected_population {
            return Err(WrfTMatrixBundleError::PopulationRole {
                role,
                expected: expected_population,
                actual: descriptor.population_role(),
            });
        }
        if descriptor.spheroid() != expected_spheroid {
            return Err(WrfTMatrixBundleError::Spheroid {
                role,
                expected: expected_spheroid,
                actual: descriptor.spheroid(),
            });
        }
        let actual_kinds = table
            .offline_lut()
            .header()
            .axes()
            .iter()
            .map(|axis| axis.kind())
            .collect::<Vec<_>>();
        let expected_kinds = match role {
            WrfTMatrixTableRole::DryOblate | WrfTMatrixTableRole::DryProlate => DRY_AXIS_KINDS,
            WrfTMatrixTableRole::WetOblate | WrfTMatrixTableRole::WetProlate => WET_AXIS_KINDS,
            WrfTMatrixTableRole::RainStandaloneAndResidual => RAIN_AXIS_KINDS,
        };
        if actual_kinds != expected_kinds {
            return Err(WrfTMatrixBundleError::AxisLayout {
                role,
                expected: expected_kinds.to_vec(),
                actual: actual_kinds,
            });
        }
        if !matches!(
            descriptor.odf(),
            TMatrixOdfConvention::GaussianCanting {
                mean_deg: 0.0,
                standard_deviation_deg: 20.0,
                alpha_quadrature_points: 5,
                beta_quadrature_points: 10,
            }
        ) {
            return Err(WrfTMatrixBundleError::Orientation { role });
        }
    }

    for role in [
        WrfTMatrixTableRole::DryOblate,
        WrfTMatrixTableRole::DryProlate,
    ] {
        let table = table_for_role(tables, role);
        if !matches!(
            table.descriptor().material(),
            TMatrixMaterial::SymmetricBruggemanSphericalAirIceMatzler2006V1 { .. }
        ) {
            return Err(WrfTMatrixBundleError::DryMaterial { role });
        }
    }
    for role in [
        WrfTMatrixTableRole::WetOblate,
        WrfTMatrixTableRole::WetProlate,
    ] {
        let table = table_for_role(tables, role);
        if !matches!(
            table.descriptor().material(),
            TMatrixMaterial::SymmetricBruggemanSphericalAirIceWaterV1 { .. }
        ) {
            return Err(WrfTMatrixBundleError::WetMaterial { role });
        }
    }
    if !matches!(
        tables.rain_standalone_and_residual.descriptor().material(),
        TMatrixMaterial::TemperatureDependentLiquidWaterLiebe1991 { .. }
    ) {
        return Err(WrfTMatrixBundleError::RainMaterial);
    }

    let reference_radar = tables.dry_oblate.descriptor().radar();
    if reference_radar.view_applicability
        != RadarViewApplicability::PpiElevationAxisMinus05To20AxisymmetricGaussian
    {
        return Err(WrfTMatrixBundleError::RadarApplicability);
    }
    for (role, table) in tables.entries().into_iter().skip(1) {
        if table.descriptor().radar() != reference_radar {
            return Err(WrfTMatrixBundleError::RadarConventionMismatch { role });
        }
    }

    let reference_frequency = axis_coordinates(
        tables.dry_oblate,
        WrfTMatrixTableRole::DryOblate,
        AxisKind::Frequency,
    )?;
    if reference_frequency != [PROPERTY_TMATRIX_FREQUENCY_HZ] {
        return Err(WrfTMatrixBundleError::FrequencyAxis {
            role: WrfTMatrixTableRole::DryOblate,
            actual: reference_frequency.to_vec(),
        });
    }
    let reference_elevations = axis_coordinates(
        tables.dry_oblate,
        WrfTMatrixTableRole::DryOblate,
        AxisKind::RadarElevation,
    )?;
    if reference_elevations.first().copied() != Some(PROPERTY_TMATRIX_MIN_ELEVATION_DEG)
        || reference_elevations.last().copied() != Some(PROPERTY_TMATRIX_MAX_ELEVATION_DEG)
    {
        return Err(WrfTMatrixBundleError::ElevationRange {
            role: WrfTMatrixTableRole::DryOblate,
            actual: reference_elevations.to_vec(),
        });
    }
    for (role, table) in tables.entries().into_iter().skip(1) {
        let frequency = axis_coordinates(table, role, AxisKind::Frequency)?;
        if frequency != reference_frequency {
            return Err(WrfTMatrixBundleError::SharedFrequencyAxis { role });
        }
        let elevations = axis_coordinates(table, role, AxisKind::RadarElevation)?;
        if elevations != reference_elevations {
            return Err(WrfTMatrixBundleError::SharedElevationAxis { role });
        }
    }

    Ok(ValidatedBundle {
        radar_elevations_deg: reference_elevations,
    })
}

fn table_for_role(
    tables: WrfTMatrixLutBundle<'_>,
    role: WrfTMatrixTableRole,
) -> &ResearchTMatrixLut {
    match role {
        WrfTMatrixTableRole::DryOblate => tables.dry_oblate,
        WrfTMatrixTableRole::DryProlate => tables.dry_prolate,
        WrfTMatrixTableRole::WetOblate => tables.wet_oblate,
        WrfTMatrixTableRole::WetProlate => tables.wet_prolate,
        WrfTMatrixTableRole::RainStandaloneAndResidual => tables.rain_standalone_and_residual,
    }
}

fn axis_coordinates(
    table: &ResearchTMatrixLut,
    role: WrfTMatrixTableRole,
    kind: AxisKind,
) -> Result<&[f64], WrfTMatrixBundleError> {
    table
        .offline_lut()
        .header()
        .axes()
        .iter()
        .find(|axis| axis.kind() == kind)
        .map(|axis| axis.coordinates())
        .ok_or(WrfTMatrixBundleError::MissingAxis { role, kind })
}

fn validate_sparse_indices(source: &WrfPropertyScene) -> Result<(), WrfTMatrixSceneBuildError> {
    for (position, &index) in source.active_cell_indices().iter().enumerate() {
        if usize::try_from(index).expect("u32 always fits usize") >= source.cell_count()
            || position > 0 && source.active_cell_indices()[position - 1] >= index
        {
            return Err(WrfTMatrixSceneBuildError::InvalidSourceIndex {
                position,
                index,
                cell_count: source.cell_count(),
            });
        }
    }
    Ok(())
}

fn usize_to_u64(value: usize) -> Result<u64, WrfTMatrixSceneBuildError> {
    u64::try_from(value).map_err(|_| WrfTMatrixSceneBuildError::AuditCountOverflow)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ElevationBracket {
    lower: usize,
    upper: usize,
    fraction: f64,
}

fn elevation_bracket(
    elevations: &[f64],
    elevation_deg: f64,
) -> Result<ElevationBracket, WrfTMatrixSceneQueryError> {
    if !elevation_deg.is_finite() {
        return Err(WrfTMatrixSceneQueryError::NonFiniteElevation { elevation_deg });
    }
    let first = *elevations
        .first()
        .ok_or(WrfTMatrixSceneQueryError::CorruptStorage)?;
    let last = *elevations
        .last()
        .ok_or(WrfTMatrixSceneQueryError::CorruptStorage)?;
    if elevation_deg < first || elevation_deg > last {
        return Err(WrfTMatrixSceneQueryError::ElevationOutsideAxis {
            elevation_deg,
            minimum_deg: first,
            maximum_deg: last,
        });
    }
    let upper = elevations.partition_point(|candidate| *candidate < elevation_deg);
    if upper < elevations.len() && elevations[upper] == elevation_deg {
        return Ok(ElevationBracket {
            lower: upper,
            upper,
            fraction: 0.0,
        });
    }
    if upper == 0 || upper == elevations.len() {
        return Err(WrfTMatrixSceneQueryError::CorruptStorage);
    }
    let lower = upper - 1;
    let fraction = (elevation_deg - elevations[lower]) / (elevations[upper] - elevations[lower]);
    Ok(ElevationBracket {
        lower,
        upper,
        fraction,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WrfTMatrixContribution {
    DryFrozen,
    WetFrozen,
    ResidualRain,
    StandaloneRain,
}

impl std::fmt::Display for WrfTMatrixContribution {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::DryFrozen => "dry frozen",
            Self::WetFrozen => "wet frozen",
            Self::ResidualRain => "residual rain",
            Self::StandaloneRain => "standalone rain",
        })
    }
}

#[derive(Debug, Error)]
pub enum WrfTMatrixSceneBuildError {
    #[error(transparent)]
    Bundle(#[from] WrfTMatrixBundleError),
    #[error(
        "property scattering grid has {cell_count} cells; u32 dense-row lookup cannot represent it"
    )]
    GridTooLarge { cell_count: usize },
    #[error(
        "property source index {index} at position {position} is unsorted, duplicated, or outside {cell_count} cells"
    )]
    InvalidSourceIndex {
        position: usize,
        index: u32,
        cell_count: usize,
    },
    #[error("close property source cell {cell_index}: {source}")]
    SourceCell {
        cell_index: usize,
        #[source]
        source: WrfPropertyReadError,
    },
    #[error("property source marks cell {cell_index} active but closes it as clear")]
    UnexpectedClearActiveCell { cell_index: usize },
    #[error("full property scattering requires rain state at cell {cell_index}: {reason}")]
    RainUnavailable {
        cell_index: usize,
        reason: RainUnavailableReason,
    },
    #[error("diagnose homogeneous mixed-phase coexistence at cell {cell_index}: {source}")]
    Coexistence {
        cell_index: usize,
        #[source]
        source: CoexistenceUnavailable,
    },
    #[error("construct exact T-matrix evaluation request: {0}")]
    EvaluationRequest(#[source] EvaluationError),
    #[error(
        "evaluate {contribution} at cell {cell_index}, elevation {elevation_deg} degrees: {source}"
    )]
    Evaluation {
        cell_index: usize,
        elevation_deg: f64,
        contribution: WrfTMatrixContribution,
        #[source]
        source: EvaluationError,
    },
    #[error(
        "accumulate {contribution} at cell {cell_index}, elevation {elevation_deg} degrees: {source}"
    )]
    Accumulation {
        cell_index: usize,
        elevation_deg: f64,
        contribution: WrfTMatrixContribution,
        #[source]
        source: OutputError,
    },
    #[error("runtime fall-moment policy for {contribution} must be {expected:?}, got {actual:?}")]
    FallMomentPolicy {
        contribution: WrfTMatrixContribution,
        expected: FallMomentPolicy,
        actual: FallMomentPolicy,
    },
    #[error("compact scattering at cell {cell_index}, elevation {elevation_deg} degrees: {source}")]
    Compact {
        cell_index: usize,
        elevation_deg: f64,
        #[source]
        source: CompactScatteringError,
    },
    #[error("diagnosed wet category unexpectedly has a conventional source")]
    WetSourceNotFrozen,
    #[error("scene storage size overflow")]
    SizeOverflow,
    #[error("scene audit-count overflow")]
    AuditCountOverflow,
    #[error("internal compact row for cell {cell_index} has {actual} values, expected {expected}")]
    InternalRowLength {
        cell_index: usize,
        expected: usize,
        actual: usize,
    },
}

#[derive(Debug, Error)]
pub enum WrfTMatrixBundleError {
    #[error("{role} table id must be {expected:?}, got {actual:?}")]
    TableId {
        role: WrfTMatrixTableRole,
        expected: &'static str,
        actual: String,
    },
    #[error("{role} table category must be {expected:?}, got {actual:?}")]
    Category {
        role: WrfTMatrixTableRole,
        expected: TMatrixParticleCategory,
        actual: TMatrixParticleCategory,
    },
    #[error("{role} population role must be {expected:?}, got {actual:?}")]
    PopulationRole {
        role: WrfTMatrixTableRole,
        expected: TMatrixPopulationRole,
        actual: TMatrixPopulationRole,
    },
    #[error("{role} spheroid convention must be {expected:?}, got {actual:?}")]
    Spheroid {
        role: WrfTMatrixTableRole,
        expected: SpheroidConvention,
        actual: SpheroidConvention,
    },
    #[error("{role} table axis layout must be {expected:?}, got {actual:?}")]
    AxisLayout {
        role: WrfTMatrixTableRole,
        expected: Vec<AxisKind>,
        actual: Vec<AxisKind>,
    },
    #[error("{role} table is missing required axis {kind:?}")]
    MissingAxis {
        role: WrfTMatrixTableRole,
        kind: AxisKind,
    },
    #[error("{role} table must use exact Gaussian20Research 5x10 ODF")]
    Orientation { role: WrfTMatrixTableRole },
    #[error("{role} table must use the property-aware dry air/ice Bruggeman material")]
    DryMaterial { role: WrfTMatrixTableRole },
    #[error("{role} table must use the property-aware air/ice/water Bruggeman material")]
    WetMaterial { role: WrfTMatrixTableRole },
    #[error("rain table must use temperature-dependent Liebe-1991 liquid water")]
    RainMaterial,
    #[error("bundle must use the exact axisymmetric PPI elevation applicability")]
    RadarApplicability,
    #[error("{role} radar convention/applicability differs from the dry-oblate reference")]
    RadarConventionMismatch { role: WrfTMatrixTableRole },
    #[error("dry-oblate frequency axis must be exactly [2.8e9], got {actual:?}")]
    FrequencyAxis {
        role: WrfTMatrixTableRole,
        actual: Vec<f64>,
    },
    #[error("{role} frequency axis differs from the exact dry-oblate axis")]
    SharedFrequencyAxis { role: WrfTMatrixTableRole },
    #[error("dry-oblate elevation axis must span exactly -0.5 through 20 degrees, got {actual:?}")]
    ElevationRange {
        role: WrfTMatrixTableRole,
        actual: Vec<f64>,
    },
    #[error("{role} elevation nodes differ from the exact dry-oblate nodes")]
    SharedElevationAxis { role: WrfTMatrixTableRole },
}

#[derive(Debug, Error)]
pub enum CompactScatteringError {
    #[error("additive component {index} value {value} is outside finite f32 storage")]
    OutsideF32 { index: usize, value: f64 },
    #[error("f32 quantization violates an additive invariant: {0}")]
    RoundTrip(#[source] OutputError),
}

#[derive(Debug, Error)]
pub enum WrfTMatrixSceneQueryError {
    #[error("property scattering cell {cell_index} is outside {cell_count} source cells")]
    CellOutOfRange {
        cell_index: usize,
        cell_count: usize,
    },
    #[error("beam elevation must be finite, got {elevation_deg}")]
    NonFiniteElevation { elevation_deg: f64 },
    #[error(
        "beam elevation {elevation_deg} is outside [{minimum_deg}, {maximum_deg}] degrees; no extrapolation or clamping is permitted"
    )]
    ElevationOutsideAxis {
        elevation_deg: f64,
        minimum_deg: f64,
        maximum_deg: f64,
    },
    #[error("property scattering compact storage is internally inconsistent")]
    CorruptStorage,
    #[error("decode/interpolate additive scattering: {0}")]
    Output(#[source] OutputError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wrf_scene_inventory::WrfSourceIdentity;

    fn additive(values: [f64; 9]) -> AdditiveScattering {
        AdditiveScattering::from_components(values).unwrap()
    }

    fn synthetic_scene() -> WrfTMatrixScene {
        let low = compact_components(additive([
            100.0, 80.0, 70.0, -5.0, -1.0, 0.1, 0.05, 500.0, 2_700.0,
        ]))
        .unwrap();
        let high = compact_components(additive([
            300.0, 240.0, 210.0, 15.0, 3.0, 0.3, 0.15, 1_800.0, 11_100.0,
        ]))
        .unwrap();
        let mut components = Vec::new();
        components.extend(low);
        components.extend(high);
        WrfTMatrixScene {
            source_identity: PropertySceneIdentity {
                source_identity: WrfSourceIdentity("fixture".to_owned()),
                time_index: 2,
            },
            microphysics_scheme_id: 50,
            required_field_signature: RequiredFieldSignature {
                microphysics_scheme_id: 50,
                fields: std::collections::BTreeSet::new(),
            },
            source_fields: Vec::new(),
            source_cell_count: 5,
            source_cell_indices: vec![3],
            full_cell_to_compact_row: vec![u32::MAX, u32::MAX, u32::MAX, 0, u32::MAX],
            radar_elevations_deg: vec![-0.5, 20.0],
            additive_components: components,
            provenance: WrfTMatrixSceneProvenance {
                status: "research_only_unvalidated",
                frequency_hz: PROPERTY_TMATRIX_FREQUENCY_HZ,
                orientation: OrientationDefinition::Gaussian20Research,
                fall_moment_policy: WrfTMatrixFallMomentAudit {
                    closed_category:
                        FallMomentPolicy::ClosedCategoryPositiveDownZeroWithinCategoryVariance,
                    diagnostic_wet_category:
                        FallMomentPolicy::DiagnosticWetCategoryPositiveDownZeroWithinCategoryVariance,
                },
                rain_mode: WrfTMatrixRainMode::FullProperty,
                tables: [
                    WrfTMatrixTableAudit {
                        role: WrfTMatrixTableRole::DryOblate,
                        table_id: DRY_OBLATE_TABLE_ID,
                        file_sha256: Sha256Digest::compute(b"a"),
                    },
                    WrfTMatrixTableAudit {
                        role: WrfTMatrixTableRole::DryProlate,
                        table_id: DRY_PROLATE_TABLE_ID,
                        file_sha256: Sha256Digest::compute(b"b"),
                    },
                    WrfTMatrixTableAudit {
                        role: WrfTMatrixTableRole::WetOblate,
                        table_id: WET_OBLATE_TABLE_ID,
                        file_sha256: Sha256Digest::compute(b"c"),
                    },
                    WrfTMatrixTableAudit {
                        role: WrfTMatrixTableRole::WetProlate,
                        table_id: WET_PROLATE_TABLE_ID,
                        file_sha256: Sha256Digest::compute(b"d"),
                    },
                    WrfTMatrixTableAudit {
                        role: WrfTMatrixTableRole::RainStandaloneAndResidual,
                        table_id: RAIN_TABLE_ID,
                        file_sha256: Sha256Digest::compute(b"e"),
                    },
                ],
                counts: WrfTMatrixAuditCounts {
                    source_cells: 1,
                    dry_frozen_populations: 1,
                    ..WrfTMatrixAuditCounts::default()
                },
            },
        }
    }

    #[test]
    fn dispatch_is_prolate_only_for_ishmael_columnar() {
        assert_eq!(
            spheroid_for_category(WrfPropertyCategory::IshmaelColumnar),
            SpheroidConvention::ProlateMajorVertical
        );
        for category in [
            WrfPropertyCategory::P3(radar_scattering::P3Category::Category1),
            WrfPropertyCategory::IshmaelPlanar,
            WrfPropertyCategory::IshmaelAggregate,
        ] {
            assert_eq!(
                spheroid_for_category(category),
                SpheroidConvention::OblateMinorVertical
            );
        }
    }

    #[test]
    fn cold_boundary_never_dispatches_zero_liquid_to_wet_table() {
        assert!(!should_diagnose_wet_coexistence(
            WrfTMatrixRainMode::FullProperty,
            true,
            true,
            DIAGNOSTIC_COEXISTENCE_COLD_K,
        ));
        assert!(should_diagnose_wet_coexistence(
            WrfTMatrixRainMode::FullProperty,
            true,
            true,
            DIAGNOSTIC_COEXISTENCE_COLD_K + 1.0e-6,
        ));
        assert!(should_diagnose_wet_coexistence(
            WrfTMatrixRainMode::FullProperty,
            true,
            true,
            DIAGNOSTIC_COEXISTENCE_WARM_K,
        ));
        assert!(!should_diagnose_wet_coexistence(
            WrfTMatrixRainMode::FullProperty,
            true,
            true,
            DIAGNOSTIC_COEXISTENCE_WARM_K + 1.0e-6,
        ));
    }

    #[test]
    fn query_interpolates_additive_components_and_preserves_signed_kdp() {
        let scene = synthetic_scene();
        let midpoint = scene.additive_at(3, 9.75).unwrap().unwrap();
        let values = midpoint.components();
        assert_eq!(values[0], 200.0);
        assert_eq!(values[1], 160.0);
        assert_eq!(values[2], 140.0);
        assert_eq!(values[3], 5.0);
        assert_eq!(values[4], 1.0);
        assert!((values[5] - 0.2).abs() < 1.0e-7);
        assert!((values[6] - 0.1).abs() < 1.0e-7);
        assert_eq!(values[7], 1_150.0);
        assert_eq!(values[8], 6_900.0);

        assert_eq!(
            scene.additive_at(3, -0.5).unwrap().unwrap().kdp().get(),
            -1.0
        );
        assert!(scene.additive_at(1, 0.0).unwrap().is_none());
    }

    #[test]
    fn query_never_clamps_or_extrapolates() {
        let scene = synthetic_scene();
        assert!(matches!(
            scene.additive_at(3, -0.500_001),
            Err(WrfTMatrixSceneQueryError::ElevationOutsideAxis { .. })
        ));
        assert!(matches!(
            scene.additive_at(3, 20.000_001),
            Err(WrfTMatrixSceneQueryError::ElevationOutsideAxis { .. })
        ));
        assert!(matches!(
            scene.additive_at(3, f64::NAN),
            Err(WrfTMatrixSceneQueryError::NonFiniteElevation { .. })
        ));
        assert!(matches!(
            scene.additive_at(5, 0.0),
            Err(WrfTMatrixSceneQueryError::CellOutOfRange { .. })
        ));
    }

    #[test]
    fn retained_memory_includes_every_owned_vector_and_identity() {
        let scene = synthetic_scene();
        let estimate = scene.memory_estimate();
        assert_eq!(estimate.source_index_bytes, size_of::<u32>());
        assert_eq!(estimate.dense_row_lookup_bytes, 5 * size_of::<u32>());
        assert_eq!(estimate.elevation_axis_bytes, 2 * size_of::<f64>());
        assert_eq!(
            estimate.additive_component_bytes,
            2 * AdditiveScattering::COMPONENT_COUNT * size_of::<f32>()
        );
        assert_eq!(estimate.source_identity_bytes, "fixture".len());
        assert_eq!(estimate.required_field_contract_bytes, 0);
        assert_eq!(estimate.source_field_provenance_bytes, 0);
        assert_eq!(estimate.provenance_text_bytes, 0);
        assert_eq!(
            estimate.retained_bytes(),
            estimate.structure_bytes
                + estimate.source_index_bytes
                + estimate.dense_row_lookup_bytes
                + estimate.elevation_axis_bytes
                + estimate.additive_component_bytes
                + estimate.source_identity_bytes
                + estimate.required_field_contract_bytes
                + estimate.source_field_provenance_bytes
                + estimate.provenance_text_bytes
        );
    }

    #[test]
    fn temporal_signature_survives_without_raw_property_arrays() {
        let scene = synthetic_scene();
        let signature = scene.temporal_signature("property-tmatrix-research-v1");
        assert_eq!(signature.microphysics_scheme_id, Some(50));
        assert_eq!(
            signature.reflectivity_source,
            "property-tmatrix-research-v1"
        );
        assert!(signature.required_raw_fields.is_empty());
        assert_eq!(scene.source_identity().time_index, 2);
        assert_eq!(scene.source_identity().source_identity.0, "fixture");
    }

    #[test]
    fn audit_count_addition_is_checked() {
        let left = WrfTMatrixAuditCounts {
            source_cells: u64::MAX,
            ..WrfTMatrixAuditCounts::default()
        };
        assert!(
            left.checked_add(WrfTMatrixAuditCounts {
                source_cells: 1,
                ..WrfTMatrixAuditCounts::default()
            })
            .is_none()
        );
    }
}
