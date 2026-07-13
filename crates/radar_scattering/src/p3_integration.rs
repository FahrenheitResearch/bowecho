//! Fail-closed integration of the exact P3 PSD over shape-authoritative nodes.
//!
//! P3 predicts maximum dimension, mass, and projected area, but it does not
//! predict a unique spheroidal axis ratio or canting distribution for its
//! dense-unrimed and partially-rimed regions. Strict mode therefore evaluates
//! only exact spheres and audits every other particle as omitted. A separate,
//! unmistakably research-only mode derives one spheroid from the native
//! mass/area law; it never labels that shape as P3 truth. Production research
//! mode also has one narrow, declared Rayleigh-limit bridge for P3's exact
//! small dense spheres below the T-matrix table's diameter floor. It is not a
//! fallback for any other table miss. All remaining shape/table omissions are
//! rejected above declared budgets.

use std::error::Error;
use std::f64::consts::PI;

use thiserror::Error;

use crate::{
    AdditiveScattering, OutputError, P3_SOLID_ICE_DENSITY_KG_M3, P3ParticleRegion,
    P3PopulationMoments, P3ProjectedAreaConsistency, P3Psd, P3PsdError, P3QuadratureAudit,
    P3QuadratureConfig, P3QuadratureNode, PsdParticleDomain, PsdSpheroidHabit,
};

pub const P3_SPHERICAL_INTEGRATION_REVISION: &str =
    "wrf-p3-v5.4-shape-authoritative-spherical-integration-v4";
pub const P3_PROJECTED_AREA_EQUIVALENT_OBLATE_REVISION: &str =
    "wrf-p3-v5.4-projected-area-equivalent-oblate-gaussian20-research-v4";
pub const P3_PROJECTED_AREA_EQUIVALENT_SPHEROID_REVISION: &str =
    "wrf-p3-v5.4-area-spheroid-fixed-point-area-closure-gaussian20-research-v3";
pub const P3_SMALL_SPHERE_RAYLEIGH_BRIDGE_REVISION: &str =
    "wrf-p3-v5.4-exact-small-dense-sphere-rayleigh-table-floor-bridge-v1";
/// Native diameter grid used by the pinned WRF source's P3-module-v4.5.2,
/// lookup-generator-v5.4 fall-speed and reflectivity moment integrations.
///
/// The source evaluates 40,000 bin centres at `j * 2 um - 1 um`. The inferred
/// bin-edge support is therefore `[0, 80 mm]`. This is source-derived
/// provenance, not a claim encoded in (or authenticated by) a runtime table
/// hash.
pub const P3_WRF_LOOKUP_INTEGRATION_BIN_COUNT: u32 = 40_000;
pub const P3_WRF_LOOKUP_INTEGRATION_BIN_WIDTH_UM: u32 = 2;
pub const P3_WRF_LOOKUP_INTEGRATION_FIRST_CENTER_UM: u32 = 1;
pub const P3_WRF_LOOKUP_INTEGRATION_LAST_CENTER_UM: u32 = 79_999;
pub const P3_WRF_LOOKUP_INTEGRATION_UPPER_EDGE_UM: u32 = 80_000;
pub const P3_WRF_LOOKUP_INTEGRATION_BIN_WIDTH_M: f64 =
    P3_WRF_LOOKUP_INTEGRATION_BIN_WIDTH_UM as f64 * 1.0e-6;
pub const P3_WRF_LOOKUP_INTEGRATION_FIRST_CENTER_M: f64 =
    P3_WRF_LOOKUP_INTEGRATION_FIRST_CENTER_UM as f64 * 1.0e-6;
pub const P3_WRF_LOOKUP_INTEGRATION_LAST_CENTER_M: f64 =
    P3_WRF_LOOKUP_INTEGRATION_LAST_CENTER_UM as f64 * 1.0e-6;
pub const P3_WRF_LOOKUP_INTEGRATION_MAXIMUM_DIMENSION_M: f64 =
    P3_WRF_LOOKUP_INTEGRATION_UPPER_EDGE_UM as f64 * 1.0e-6;
pub const P3_WRF_LOOKUP_INTEGRATION_SOURCE_PATH: &str = "run/create_p3_lookupTable_1.f90-v5.4";
pub const P3_WRF_LOOKUP_INTEGRATION_DOMAIN_REVISION: &str =
    "wrf-p3-v5.4-lookup-integration-grid-inferred-v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum P3TMatrixSourceDomainAuthority {
    /// Inferred from the pinned generator's 40,000-bin, 2 um numerical grid.
    InferredPinnedWrfLookupIntegrationGridV1,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct P3TMatrixSourceDomainProvenance {
    pub authority: P3TMatrixSourceDomainAuthority,
    pub revision: &'static str,
    pub wrf_source_commit: &'static str,
    pub source_path: &'static str,
    pub bin_count: u32,
    pub bin_width_m: f64,
    pub first_bin_center_m: f64,
    pub last_bin_center_m: f64,
    pub maximum_dimension_edge_m: f64,
}

pub const P3_TMATRIX_SOURCE_DOMAIN_PROVENANCE: P3TMatrixSourceDomainProvenance =
    P3TMatrixSourceDomainProvenance {
        authority: P3TMatrixSourceDomainAuthority::InferredPinnedWrfLookupIntegrationGridV1,
        revision: P3_WRF_LOOKUP_INTEGRATION_DOMAIN_REVISION,
        wrf_source_commit: crate::P3_WRF_SOURCE_COMMIT,
        source_path: P3_WRF_LOOKUP_INTEGRATION_SOURCE_PATH,
        bin_count: P3_WRF_LOOKUP_INTEGRATION_BIN_COUNT,
        bin_width_m: P3_WRF_LOOKUP_INTEGRATION_BIN_WIDTH_M,
        first_bin_center_m: P3_WRF_LOOKUP_INTEGRATION_FIRST_CENTER_M,
        last_bin_center_m: P3_WRF_LOOKUP_INTEGRATION_LAST_CENTER_M,
        maximum_dimension_edge_m: P3_WRF_LOOKUP_INTEGRATION_MAXIMUM_DIMENSION_M,
    };

/// Shape contract applied after exact native P3 lambda/mu reconstruction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum P3TMatrixShapePolicy {
    /// Evaluate only regions that the pinned P3 law itself defines as
    /// spheres; all other nodes count against the omission budget.
    StrictShapeAuthoritativeSpheres,
    /// Research-only equivalent oblate: horizontal major diameter is P3
    /// Dmax and the vertical minor diameter is chosen so its side projected
    /// area equals P3 A(D). The existing Gaussian-20 table ODF is an explicit
    /// external assumption, not a P3-predicted canting distribution.
    ProjectedAreaEquivalentOblateGaussian20ResearchV1,
    /// Research-only projected-area-equivalent spheroid. The usual P3 area
    /// maps to an oblate with horizontal major diameter Dmax. A typed native
    /// partially-rimed transition artifact caused by the pinned rime-density
    /// fixed point is closed to the Dmax sphere for scattering while its raw
    /// source area remains recorded and audited. This preserves mass and P3's
    /// actual maximum dimension instead of inventing a larger prolate axis.
    /// Any unmarked area above the Dmax circle remains a shape omission. Where the
    /// empirical P3 area-law transition would otherwise imply a homogeneous
    /// density above P3's own 900 kg/m3 solid-ice density, the mapping instead
    /// preserves particle mass at that physical density bound and retains the
    /// native normalized area as its aspect-ratio proxy.
    ProjectedAreaEquivalentSpheroidGaussian20ResearchV1,
}

/// Explicit treatment of exact P3 small dense spheres below a particle LUT's
/// diameter floor. No policy applies to nonspherical particles or to any
/// upper-size, density, aspect-ratio, temperature, frequency, or view miss.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum P3SmallSphereScatteringPolicy {
    Disabled,
    RayleighLimitBelowTableDiameterFloorV1,
}

/// Per-node scattering route selected before the application evaluator runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum P3ParticleScatteringRoute {
    TMatrixTable,
    TableFloorAnchoredSmallDenseSphereRayleighV1,
}

/// Explicit omission gates for the only shape-authoritative P3/T-matrix seam.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct P3TMatrixIntegrationConfig {
    shape_policy: P3TMatrixShapePolicy,
    small_sphere_policy: P3SmallSphereScatteringPolicy,
    quadrature: P3QuadratureConfig,
    maximum_omitted_number_fraction: f64,
    maximum_omitted_mass_fraction: f64,
    maximum_omitted_radar_weight_fraction: f64,
}

impl P3TMatrixIntegrationConfig {
    pub fn new(
        shape_policy: P3TMatrixShapePolicy,
        small_sphere_policy: P3SmallSphereScatteringPolicy,
        quadrature: P3QuadratureConfig,
        maximum_omitted_number_fraction: f64,
        maximum_omitted_mass_fraction: f64,
        maximum_omitted_radar_weight_fraction: f64,
    ) -> Result<Self, P3TMatrixIntegrationConfigError> {
        for (field, value) in [
            (
                "maximum omitted P3 number fraction",
                maximum_omitted_number_fraction,
            ),
            (
                "maximum omitted P3 mass fraction",
                maximum_omitted_mass_fraction,
            ),
            (
                "maximum omitted P3 mass-squared radar-weight fraction",
                maximum_omitted_radar_weight_fraction,
            ),
        ] {
            if !value.is_finite() || !(0.0..1.0).contains(&value) {
                return Err(P3TMatrixIntegrationConfigError::InvalidFraction { field, value });
            }
        }
        Ok(Self {
            shape_policy,
            small_sphere_policy,
            quadrature,
            maximum_omitted_number_fraction,
            maximum_omitted_mass_fraction,
            maximum_omitted_radar_weight_fraction,
        })
    }

    #[must_use]
    pub const fn shape_policy(self) -> P3TMatrixShapePolicy {
        self.shape_policy
    }

    #[must_use]
    pub const fn small_sphere_policy(self) -> P3SmallSphereScatteringPolicy {
        self.small_sphere_policy
    }

    #[must_use]
    pub const fn quadrature(self) -> P3QuadratureConfig {
        self.quadrature
    }

    #[must_use]
    pub const fn maximum_omitted_number_fraction(self) -> f64 {
        self.maximum_omitted_number_fraction
    }

    #[must_use]
    pub const fn maximum_omitted_mass_fraction(self) -> f64 {
        self.maximum_omitted_mass_fraction
    }

    #[must_use]
    pub const fn maximum_omitted_radar_weight_fraction(self) -> f64 {
        self.maximum_omitted_radar_weight_fraction
    }
}

impl Default for P3TMatrixIntegrationConfig {
    fn default() -> Self {
        Self::new(
            P3TMatrixShapePolicy::StrictShapeAuthoritativeSpheres,
            P3SmallSphereScatteringPolicy::Disabled,
            P3QuadratureConfig::default(),
            0.999,
            0.05,
            0.001,
        )
        .expect("the versioned P3 T-matrix integration config is valid")
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq)]
pub enum P3TMatrixIntegrationConfigError {
    #[error("{field} must be finite and within [0, 1), got {value}")]
    InvalidFraction { field: &'static str, value: f64 },
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct P3TMatrixWeightFractions {
    pub number: f64,
    pub mass: f64,
    pub mass_squared_radar_weight: f64,
    pub sixth_moment: f64,
}

/// Analytic population excluded before any spheroid mapping because it lies
/// beyond the pinned WRF lookup generator's inferred moment-integration
/// domain. These values are informational source-contract audit data, not part
/// of the in-source shape/table omission gate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct P3TMatrixSourceDomainAudit {
    pub provenance: P3TMatrixSourceDomainProvenance,
    pub in_source_nodes: usize,
    pub analytic_total: P3PopulationMoments,
    pub source_represented: P3PopulationMoments,
    pub source_excluded: P3PopulationMoments,
    pub source_excluded_fraction_of_analytic_psd: P3TMatrixWeightFractions,
    pub source_quadrature_relative_error: P3TMatrixWeightFractions,
}

/// Shape/table omissions inside the inferred WRF source integration domain.
/// Every fraction uses the corresponding in-source population as its
/// denominator; source-excluded nodes never consume this gate's budget.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct P3TMatrixInSourceOmissionAudit {
    pub non_spherical_number_fraction: f64,
    pub non_spherical_mass_fraction: f64,
    pub non_spherical_radar_weight_fraction: f64,
    pub outside_table_number_fraction: f64,
    pub outside_table_mass_fraction: f64,
    pub outside_table_radar_weight_fraction: f64,
    pub total_omitted_number_fraction: f64,
    pub total_omitted_mass_fraction: f64,
    pub total_omitted_radar_weight_fraction: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct P3TMatrixIntegrationAudit {
    pub revision: &'static str,
    pub config: P3TMatrixIntegrationConfig,
    pub quadrature: P3QuadratureAudit,
    pub supported_nodes: usize,
    pub non_spherical_nodes: usize,
    pub outside_table_nodes: usize,
    pub projected_area_equivalent_nodes: usize,
    pub solid_ice_constrained_nodes: usize,
    pub fixed_point_area_closure_nodes: usize,
    pub maximum_fixed_point_raw_area_ratio: Option<f64>,
    pub fixed_point_area_closure_radar_weight_fraction: f64,
    pub small_sphere_rayleigh_bridge_nodes: usize,
    pub source_domain: P3TMatrixSourceDomainAudit,
    pub in_source_shape_table_omission: P3TMatrixInSourceOmissionAudit,
}

#[derive(Clone, Debug, PartialEq)]
pub struct P3TMatrixIntegrationResult {
    additive: AdditiveScattering,
    audit: P3TMatrixIntegrationAudit,
}

impl P3TMatrixIntegrationResult {
    #[must_use]
    pub const fn additive(&self) -> AdditiveScattering {
        self.additive
    }

    #[must_use]
    pub const fn audit(&self) -> P3TMatrixIntegrationAudit {
        self.audit
    }
}

#[derive(Debug, Error)]
pub enum P3TMatrixIntegrationError<E: Error + 'static> {
    #[error("construct exact P3 quadrature: {0}")]
    Psd(#[source] P3PsdError),
    #[error(
        "P3 cannot be represented inside the inferred WRF source integration domain by the existing spheroidal tables without an invented shape: in-source omitted number={number_fraction}, mass={mass_fraction}, equivalent-ice-volume-squared radar weight={radar_weight_fraction} (shape={shape_radar_weight_fraction}, table={table_radar_weight_fraction}); in-source limits are {maximum_number}, {maximum_mass}, {maximum_radar_weight}"
    )]
    ShapeOrTableOmission {
        number_fraction: f64,
        mass_fraction: f64,
        radar_weight_fraction: f64,
        shape_radar_weight_fraction: f64,
        table_radar_weight_fraction: f64,
        maximum_number: f64,
        maximum_mass: f64,
        maximum_radar_weight: f64,
    },
    #[error("P3 projected-area equivalent geometry is invalid at node {node_index}: {message}")]
    InvalidEquivalentGeometry { node_index: usize, message: String },
    #[error("evaluate table-ready P3 quadrature node {node_index}: {source}")]
    Evaluation {
        node_index: usize,
        #[source]
        source: E,
    },
    #[error("scale or accumulate table-ready P3 quadrature node {node_index}: {source}")]
    Output {
        node_index: usize,
        #[source]
        source: OutputError,
    },
}

/// One table-ready particle. Exact-sphere nodes have ratio one; research-mode
/// nodes retain the explicit projected-area-equivalent oblate mapping.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct P3TMatrixParticleNode {
    source: P3QuadratureNode,
    equivolume_diameter_m: f64,
    bulk_density_kg_m3: f64,
    minor_to_major_axis_ratio: f64,
    habit: PsdSpheroidHabit,
    shape_policy: P3TMatrixShapePolicy,
    scattering_route: P3ParticleScatteringRoute,
    solid_ice_constrained: bool,
    fixed_point_area_closed: bool,
    source_projected_area_ratio: f64,
}

impl P3TMatrixParticleNode {
    #[must_use]
    pub const fn source(self) -> P3QuadratureNode {
        self.source
    }

    #[must_use]
    pub const fn equivolume_diameter_m(self) -> f64 {
        self.equivolume_diameter_m
    }

    #[must_use]
    pub const fn bulk_density_kg_m3(self) -> f64 {
        self.bulk_density_kg_m3
    }

    #[must_use]
    pub const fn minor_to_major_axis_ratio(self) -> f64 {
        self.minor_to_major_axis_ratio
    }

    #[must_use]
    pub const fn habit(self) -> PsdSpheroidHabit {
        self.habit
    }

    #[must_use]
    pub const fn shape_policy(self) -> P3TMatrixShapePolicy {
        self.shape_policy
    }

    #[must_use]
    pub const fn scattering_route(self) -> P3ParticleScatteringRoute {
        self.scattering_route
    }

    #[must_use]
    pub const fn solid_ice_constrained(self) -> bool {
        self.solid_ice_constrained
    }

    #[must_use]
    pub const fn fixed_point_area_closed(self) -> bool {
        self.fixed_point_area_closed
    }

    #[must_use]
    pub const fn source_projected_area_ratio(self) -> f64 {
        self.source_projected_area_ratio
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct OmittedWeights {
    number: f64,
    mass: f64,
    radar_weight: f64,
    nodes: usize,
}

impl OmittedWeights {
    fn add(&mut self, node: &P3QuadratureNode) {
        self.number += node.number_concentration_m3;
        self.mass += node.number_concentration_m3 * node.particle.mass_kg;
        self.radar_weight += p3_radar_weight(node);
        self.nodes += 1;
    }
}

/// P3's official lookup-table generator weights its Rayleigh-equivalent ice
/// reflectivity by `(m / rho_ice)^2`, not by maximum-dimension sixth moment.
/// The constant ice-density factor cancels in an omitted fraction, leaving
/// `N m^2`. This matters for large, fluffy P3 particles: `Dmax^6` would assign
/// them the reflectivity of solid constant-density spheres and can overstate
/// an unsupported tail by orders of magnitude.
fn p3_radar_weight(node: &P3QuadratureNode) -> f64 {
    node.number_concentration_m3 * node.particle.mass_kg.powi(2)
}

/// Integrate exact native P3 PSD weights through an explicit shape policy.
///
/// The callback returns scattering normalized to one particle per cubic
/// metre. Population scaling is applied exactly once here. Strict mode never
/// maps nonspherical nodes. Research mode applies only the named projected-
/// area equivalent mapping; table-domain misses are never clipped to an edge.
pub fn integrate_p3_tmatrix_psd<E, F>(
    distribution: &P3Psd,
    config: P3TMatrixIntegrationConfig,
    table_support: crate::PsdParticleSupport,
    mut evaluate_one_particle_per_m3: F,
) -> Result<P3TMatrixIntegrationResult, P3TMatrixIntegrationError<E>>
where
    E: Error + 'static,
    F: FnMut(&P3TMatrixParticleNode) -> Result<AdditiveScattering, E>,
{
    let mut diameter_breakpoints = Vec::with_capacity(4);
    for domain in [table_support.spherical(), table_support.oblate()]
        .into_iter()
        .flatten()
    {
        diameter_breakpoints.extend(domain.equivolume_diameter_range_m());
    }
    let source_upper_m = P3_WRF_LOOKUP_INTEGRATION_MAXIMUM_DIMENSION_M;
    let analytic_total = distribution
        .analytic_population_moments(0.0, f64::INFINITY)
        .map_err(P3TMatrixIntegrationError::Psd)?;
    let source_represented = distribution
        .analytic_population_moments(0.0, source_upper_m)
        .map_err(P3TMatrixIntegrationError::Psd)?;
    let source_excluded = distribution
        .analytic_population_moments(source_upper_m, f64::INFINITY)
        .map_err(P3TMatrixIntegrationError::Psd)?;
    let quadrature = distribution
        .quadrature_bounded_with_dimension_breakpoints(
            config.quadrature,
            source_upper_m,
            &diameter_breakpoints,
        )
        .map_err(P3TMatrixIntegrationError::Psd)?;

    let mut non_spherical = OmittedWeights::default();
    let mut outside_table = OmittedWeights::default();
    let mut supported = Vec::new();
    let mut projected_area_equivalent_nodes = 0;
    let mut solid_ice_constrained_nodes = 0;
    let mut fixed_point_area_closure = OmittedWeights::default();
    let mut maximum_fixed_point_raw_area_ratio: Option<f64> = None;
    let mut small_sphere_rayleigh_bridge_nodes = 0;
    for (index, node) in quadrature.nodes.iter().enumerate() {
        let mut table_node = match config.shape_policy {
            P3TMatrixShapePolicy::StrictShapeAuthoritativeSpheres => {
                if !node.particle.is_exact_sphere() {
                    non_spherical.add(node);
                    continue;
                }
                let density = if node.particle.region == P3ParticleRegion::SmallDenseSphere {
                    P3_SOLID_ICE_DENSITY_KG_M3
                } else {
                    node.particle.effective_spherical_density_kg_m3
                };
                P3TMatrixParticleNode {
                    source: *node,
                    equivolume_diameter_m: node.particle.maximum_dimension_m,
                    bulk_density_kg_m3: density,
                    minor_to_major_axis_ratio: 1.0,
                    habit: PsdSpheroidHabit::Spherical,
                    shape_policy: config.shape_policy,
                    scattering_route: P3ParticleScatteringRoute::TMatrixTable,
                    solid_ice_constrained: false,
                    fixed_point_area_closed: false,
                    source_projected_area_ratio: 1.0,
                }
            }
            P3TMatrixShapePolicy::ProjectedAreaEquivalentOblateGaussian20ResearchV1 => {
                let Ok(table_node) = projected_area_equivalent_oblate(*node) else {
                    // A source mass/area tuple that cannot define the named
                    // equivalent spheroid is an in-source shape omission. It
                    // is gated together with table omissions instead of being
                    // clamped into an invented particle or aborting before the
                    // declared omission policy can evaluate it.
                    non_spherical.add(node);
                    continue;
                };
                projected_area_equivalent_nodes += 1;
                table_node
            }
            P3TMatrixShapePolicy::ProjectedAreaEquivalentSpheroidGaussian20ResearchV1 => {
                let Ok(table_node) = projected_area_equivalent_spheroid(*node) else {
                    non_spherical.add(node);
                    continue;
                };
                projected_area_equivalent_nodes += 1;
                table_node
            }
        };
        solid_ice_constrained_nodes += usize::from(table_node.solid_ice_constrained);
        if table_node.fixed_point_area_closed {
            fixed_point_area_closure.add(node);
            maximum_fixed_point_raw_area_ratio = Some(
                maximum_fixed_point_raw_area_ratio
                    .map_or(table_node.source_projected_area_ratio, |maximum| {
                        maximum.max(table_node.source_projected_area_ratio)
                    }),
            );
        }
        if small_sphere_bridge_applies(config, *node, table_node, table_support) {
            table_node.scattering_route =
                P3ParticleScatteringRoute::TableFloorAnchoredSmallDenseSphereRayleighV1;
            small_sphere_rayleigh_bridge_nodes += 1;
            supported.push((index, table_node));
            continue;
        }
        let inside_table = table_support
            .domain_for(table_node.habit)
            .is_some_and(|domain| table_node_in_domain(table_node, domain));
        if inside_table {
            supported.push((index, table_node));
        } else {
            outside_table.add(node);
        }
    }

    // Shape/table adequacy is judged only against the population represented
    // by the pinned source moment grid. The full analytic PSD remains intact
    // and its >80 mm population is reported independently below.
    let non_spherical_fractions = fractions(
        non_spherical,
        source_represented.number_density_m3,
        source_represented.mass_concentration_kg_m3,
        source_represented.mass_squared_radar_weight_kg2_m3,
    );
    let outside_table_fractions = fractions(
        outside_table,
        source_represented.number_density_m3,
        source_represented.mass_concentration_kg_m3,
        source_represented.mass_squared_radar_weight_kg2_m3,
    );
    let total_fractions = [
        non_spherical_fractions[0] + outside_table_fractions[0],
        non_spherical_fractions[1] + outside_table_fractions[1],
        non_spherical_fractions[2] + outside_table_fractions[2],
    ];
    if total_fractions[0] > config.maximum_omitted_number_fraction
        || total_fractions[1] > config.maximum_omitted_mass_fraction
        || total_fractions[2] > config.maximum_omitted_radar_weight_fraction
    {
        return Err(P3TMatrixIntegrationError::ShapeOrTableOmission {
            number_fraction: total_fractions[0],
            mass_fraction: total_fractions[1],
            radar_weight_fraction: total_fractions[2],
            shape_radar_weight_fraction: non_spherical_fractions[2],
            table_radar_weight_fraction: outside_table_fractions[2],
            maximum_number: config.maximum_omitted_number_fraction,
            maximum_mass: config.maximum_omitted_mass_fraction,
            maximum_radar_weight: config.maximum_omitted_radar_weight_fraction,
        });
    }

    let mut additive = AdditiveScattering::default();
    for &(node_index, node) in &supported {
        let per_particle = evaluate_one_particle_per_m3(&node)
            .map_err(|source| P3TMatrixIntegrationError::Evaluation { node_index, source })?;
        let scaled = per_particle
            .checked_scale(node.source.number_concentration_m3)
            .map_err(|source| P3TMatrixIntegrationError::Output { node_index, source })?;
        additive = additive
            .checked_add(scaled)
            .map_err(|source| P3TMatrixIntegrationError::Output { node_index, source })?;
    }

    Ok(P3TMatrixIntegrationResult {
        additive,
        audit: P3TMatrixIntegrationAudit {
            revision: match config.shape_policy {
                P3TMatrixShapePolicy::StrictShapeAuthoritativeSpheres => {
                    P3_SPHERICAL_INTEGRATION_REVISION
                }
                P3TMatrixShapePolicy::ProjectedAreaEquivalentOblateGaussian20ResearchV1 => {
                    P3_PROJECTED_AREA_EQUIVALENT_OBLATE_REVISION
                }
                P3TMatrixShapePolicy::ProjectedAreaEquivalentSpheroidGaussian20ResearchV1 => {
                    P3_PROJECTED_AREA_EQUIVALENT_SPHEROID_REVISION
                }
            },
            config,
            quadrature: quadrature.audit,
            supported_nodes: supported.len(),
            non_spherical_nodes: non_spherical.nodes,
            outside_table_nodes: outside_table.nodes,
            projected_area_equivalent_nodes,
            solid_ice_constrained_nodes,
            fixed_point_area_closure_nodes: fixed_point_area_closure.nodes,
            maximum_fixed_point_raw_area_ratio,
            fixed_point_area_closure_radar_weight_fraction: fixed_point_area_closure.radar_weight
                / source_represented.mass_squared_radar_weight_kg2_m3,
            small_sphere_rayleigh_bridge_nodes,
            source_domain: P3TMatrixSourceDomainAudit {
                provenance: P3_TMATRIX_SOURCE_DOMAIN_PROVENANCE,
                in_source_nodes: quadrature.nodes.len(),
                analytic_total,
                source_represented,
                source_excluded,
                source_excluded_fraction_of_analytic_psd: moment_fractions(
                    source_excluded,
                    analytic_total,
                ),
                source_quadrature_relative_error: P3TMatrixWeightFractions {
                    number: quadrature.audit.number_quadrature_relative_error,
                    mass: quadrature.audit.mass_quadrature_relative_error,
                    mass_squared_radar_weight: quadrature
                        .audit
                        .mass_squared_radar_weight_quadrature_relative_error,
                    sixth_moment: quadrature.audit.sixth_moment_quadrature_relative_error,
                },
            },
            in_source_shape_table_omission: P3TMatrixInSourceOmissionAudit {
                non_spherical_number_fraction: non_spherical_fractions[0],
                non_spherical_mass_fraction: non_spherical_fractions[1],
                non_spherical_radar_weight_fraction: non_spherical_fractions[2],
                outside_table_number_fraction: outside_table_fractions[0],
                outside_table_mass_fraction: outside_table_fractions[1],
                outside_table_radar_weight_fraction: outside_table_fractions[2],
                total_omitted_number_fraction: total_fractions[0],
                total_omitted_mass_fraction: total_fractions[1],
                total_omitted_radar_weight_fraction: total_fractions[2],
            },
        },
    })
}

fn small_sphere_bridge_applies(
    config: P3TMatrixIntegrationConfig,
    source: P3QuadratureNode,
    mapped: P3TMatrixParticleNode,
    table_support: crate::PsdParticleSupport,
) -> bool {
    if config.small_sphere_policy
        != P3SmallSphereScatteringPolicy::RayleighLimitBelowTableDiameterFloorV1
        || source.particle.region != P3ParticleRegion::SmallDenseSphere
        || !source.particle.is_exact_sphere()
        || mapped.habit != PsdSpheroidHabit::Spherical
        || mapped.bulk_density_kg_m3.to_bits() != P3_SOLID_ICE_DENSITY_KG_M3.to_bits()
        || mapped.minor_to_major_axis_ratio.to_bits() != 1.0_f64.to_bits()
    {
        return false;
    }
    let Some(domain) = table_support.spherical() else {
        return false;
    };
    mapped.equivolume_diameter_m < domain.equivolume_diameter_range_m()[0]
        && contains(domain.bulk_density_range_kg_m3(), mapped.bulk_density_kg_m3)
        && contains(
            domain.minor_to_major_axis_ratio_range(),
            mapped.minor_to_major_axis_ratio,
        )
}

fn projected_area_equivalent_oblate(
    node: P3QuadratureNode,
) -> Result<P3TMatrixParticleNode, &'static str> {
    projected_area_equivalent_spheroid_impl(
        node,
        P3TMatrixShapePolicy::ProjectedAreaEquivalentOblateGaussian20ResearchV1,
        false,
    )
}

fn projected_area_equivalent_spheroid(
    node: P3QuadratureNode,
) -> Result<P3TMatrixParticleNode, &'static str> {
    projected_area_equivalent_spheroid_impl(
        node,
        P3TMatrixShapePolicy::ProjectedAreaEquivalentSpheroidGaussian20ResearchV1,
        true,
    )
}

fn projected_area_equivalent_spheroid_impl(
    node: P3QuadratureNode,
    shape_policy: P3TMatrixShapePolicy,
    permit_fixed_point_area_closure: bool,
) -> Result<P3TMatrixParticleNode, &'static str> {
    let maximum_dimension = node.particle.maximum_dimension_m;
    let raw_ratio = 4.0 * node.particle.projected_area_m2 / (PI * maximum_dimension.powi(2));
    if !raw_ratio.is_finite() || raw_ratio <= 0.0 {
        return Err("4A/(pi Dmax^2) is nonpositive or nonfinite");
    }
    let roundoff_limit = 1.0 + 1.0e-10;
    // P3's one-percent convergence test applies to the graupel coefficient,
    // while its last partially-rimed breakpoint is still based on the prior
    // coefficient. The resulting source artifact is not bounded to one
    // percent in area (the error is amplified at low rime fraction). Only the
    // typed geometry produced by that pinned source sequence may use this
    // closure. A prolate that preserved the impossible raw area would require
    // a major axis larger than P3's declared Dmax, so retain the raw area for
    // audit and use the mass-preserving Dmax sphere for scattering.
    let fixed_point_area_closed = permit_fixed_point_area_closure
        && raw_ratio > roundoff_limit
        && node.particle.region == P3ParticleRegion::PartiallyRimed
        && node.particle.projected_area_consistency
            == P3ProjectedAreaConsistency::PinnedFinalCoefficientTransitionOvershoot;
    if raw_ratio > roundoff_limit && !fixed_point_area_closed {
        return Err("4A/(pi Dmax^2) exceeds the selected equivalent-spheroid authority");
    }
    let roundoff_sphere = raw_ratio > 1.0 && raw_ratio <= roundoff_limit;
    let geometry_ratio = if roundoff_sphere || fixed_point_area_closed {
        1.0
    } else {
        raw_ratio
    };
    let (habit, axis_ratio) =
        if roundoff_sphere || fixed_point_area_closed || (raw_ratio - 1.0).abs() <= 1.0e-10 {
            (PsdSpheroidHabit::Spherical, 1.0)
        } else {
            (PsdSpheroidHabit::Oblate, raw_ratio)
        };
    let area_equivalent_diameter = maximum_dimension * geometry_ratio.cbrt();
    let area_equivalent_volume = PI / 6.0 * area_equivalent_diameter.powi(3);
    let area_equivalent_density = node.particle.mass_kg / area_equivalent_volume;
    if fixed_point_area_closed
        && area_equivalent_density > P3_SOLID_ICE_DENSITY_KG_M3 * (1.0 + 1.0e-10)
    {
        return Err("fixed-point area closure exceeds P3 solid-ice density");
    }
    // P3's empirical unrimed area law changes at the exact small-sphere
    // boundary. Treating that projected-area fill factor as the volume of a
    // homogeneous spheroid can then demand more than P3's own solid-ice
    // density. Preserve mass and the normalized-area aspect proxy, but expand
    // the equivalent volume just enough to remain at the source's physical
    // solid-ice density. This is continuous where the two constructions meet
    // and is not a clamp to an EM-table coordinate.
    let exact_small_dense_sphere = node.particle.region == P3ParticleRegion::SmallDenseSphere
        && node.particle.is_exact_sphere();
    let solid_ice_constrained = !exact_small_dense_sphere
        && area_equivalent_density > P3_SOLID_ICE_DENSITY_KG_M3 * (1.0 + 1.0e-10);
    let (equivolume_diameter, density) = if exact_small_dense_sphere {
        (maximum_dimension, P3_SOLID_ICE_DENSITY_KG_M3)
    } else if solid_ice_constrained {
        (
            (6.0 * node.particle.mass_kg / (PI * P3_SOLID_ICE_DENSITY_KG_M3)).cbrt(),
            P3_SOLID_ICE_DENSITY_KG_M3,
        )
    } else {
        (area_equivalent_diameter, area_equivalent_density)
    };
    if !equivolume_diameter.is_finite()
        || equivolume_diameter <= 0.0
        || !density.is_finite()
        || density <= 0.0
    {
        return Err("derived equivalent diameter or density is nonpositive or nonfinite");
    }
    Ok(P3TMatrixParticleNode {
        source: node,
        equivolume_diameter_m: equivolume_diameter,
        bulk_density_kg_m3: density,
        minor_to_major_axis_ratio: axis_ratio,
        habit,
        shape_policy,
        scattering_route: P3ParticleScatteringRoute::TMatrixTable,
        solid_ice_constrained,
        fixed_point_area_closed,
        source_projected_area_ratio: raw_ratio,
    })
}

fn table_node_in_domain(node: P3TMatrixParticleNode, domain: PsdParticleDomain) -> bool {
    contains(
        domain.equivolume_diameter_range_m(),
        node.equivolume_diameter_m,
    ) && contains(domain.bulk_density_range_kg_m3(), node.bulk_density_kg_m3)
        && contains(
            domain.minor_to_major_axis_ratio_range(),
            node.minor_to_major_axis_ratio,
        )
}

fn fractions(
    omitted: OmittedWeights,
    total_number: f64,
    total_mass: f64,
    total_radar_weight: f64,
) -> [f64; 3] {
    [
        omitted.number / total_number,
        omitted.mass / total_mass,
        omitted.radar_weight / total_radar_weight,
    ]
}

fn moment_fractions(
    numerator: P3PopulationMoments,
    denominator: P3PopulationMoments,
) -> P3TMatrixWeightFractions {
    P3TMatrixWeightFractions {
        number: numerator.number_density_m3 / denominator.number_density_m3,
        mass: numerator.mass_concentration_kg_m3 / denominator.mass_concentration_kg_m3,
        mass_squared_radar_weight: numerator.mass_squared_radar_weight_kg2_m3
            / denominator.mass_squared_radar_weight_kg2_m3,
        sixth_moment: numerator.sixth_moment_m3 / denominator.sixth_moment_m3,
    }
}

fn contains(range: [f64; 2], value: f64) -> bool {
    value >= range[0] && value <= range[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        P3ParticleGeometry, P3ParticleRegion, P3ProjectedAreaConsistency, P3ShapeAuthority,
    };

    fn node(maximum_dimension_m: f64, mass_kg: f64, number_m3: f64) -> P3QuadratureNode {
        P3QuadratureNode {
            maximum_dimension_m,
            number_concentration_m3: number_m3,
            particle: P3ParticleGeometry {
                maximum_dimension_m,
                mass_kg,
                projected_area_m2: PI / 4.0 * maximum_dimension_m.powi(2),
                effective_spherical_density_kg_m3: mass_kg
                    / (PI / 6.0 * maximum_dimension_m.powi(3)),
                region: P3ParticleRegion::DenseUnrimed,
                shape_authority: P3ShapeAuthority::MaximumDimensionAndProjectedAreaOnly,
                projected_area_consistency: P3ProjectedAreaConsistency::GeometricallyBounded,
            },
        }
    }

    #[test]
    fn radar_omission_weight_is_scheme_mass_squared_not_dmax_sixth() {
        let fluffy = node(0.1, 1.0e-9, 10.0);
        let compact = node(0.001, 1.0e-6, 1.0);
        let mut omitted = OmittedWeights::default();
        omitted.add(&fluffy);
        let total_radar_weight = p3_radar_weight(&fluffy) + p3_radar_weight(&compact);
        let omitted_fraction = fractions(omitted, 11.0, 1.01e-6, total_radar_weight)[2];
        let dmax_sixth_fraction =
            10.0 * 0.1_f64.powi(6) / (10.0 * 0.1_f64.powi(6) + 0.001_f64.powi(6));

        assert!(omitted_fraction < 1.0e-4);
        assert!(dmax_sixth_fraction > 0.999);
        assert_eq!(p3_radar_weight(&fluffy), 10.0 * 1.0e-9_f64.powi(2));
    }

    #[test]
    fn pinned_lookup_integration_grid_has_exact_eighty_mm_upper_edge() {
        assert_eq!(P3_WRF_LOOKUP_INTEGRATION_BIN_COUNT, 40_000);
        assert_eq!(P3_WRF_LOOKUP_INTEGRATION_BIN_WIDTH_UM, 2);
        assert_eq!(P3_WRF_LOOKUP_INTEGRATION_FIRST_CENTER_UM, 1);
        assert_eq!(P3_WRF_LOOKUP_INTEGRATION_LAST_CENTER_UM, 79_999);
        assert_eq!(P3_WRF_LOOKUP_INTEGRATION_UPPER_EDGE_UM, 80_000);
        assert_eq!(
            P3_WRF_LOOKUP_INTEGRATION_BIN_COUNT * P3_WRF_LOOKUP_INTEGRATION_BIN_WIDTH_UM,
            P3_WRF_LOOKUP_INTEGRATION_UPPER_EDGE_UM
        );
        assert_eq!(
            P3_WRF_LOOKUP_INTEGRATION_LAST_CENTER_UM,
            P3_WRF_LOOKUP_INTEGRATION_UPPER_EDGE_UM - P3_WRF_LOOKUP_INTEGRATION_BIN_WIDTH_UM / 2
        );
        assert_eq!(
            P3_WRF_LOOKUP_INTEGRATION_MAXIMUM_DIMENSION_M.to_bits(),
            0.080_f64.to_bits()
        );
    }

    #[test]
    fn source_exclusion_fractions_use_full_analytic_denominators() {
        let full = P3PopulationMoments {
            number_density_m3: 100.0,
            mass_concentration_kg_m3: 20.0,
            mass_squared_radar_weight_kg2_m3: 10.0,
            sixth_moment_m3: 5.0,
        };
        let excluded = P3PopulationMoments {
            number_density_m3: 10.0,
            mass_concentration_kg_m3: 4.0,
            mass_squared_radar_weight_kg2_m3: 1.0,
            sixth_moment_m3: 1.0,
        };
        assert_eq!(
            moment_fractions(excluded, full),
            P3TMatrixWeightFractions {
                number: 0.1,
                mass: 0.2,
                mass_squared_radar_weight: 0.1,
                sixth_moment: 0.2,
            }
        );
    }

    #[test]
    fn unmarked_projected_area_above_circumscribing_circle_is_rejected() {
        let mut invalid = node(0.01, 1.0e-9, 1.0);
        invalid.particle.projected_area_m2 *= 1.001;
        assert!(projected_area_equivalent_oblate(invalid).is_err());
        assert!(projected_area_equivalent_spheroid(invalid).is_err());
    }

    #[test]
    fn pinned_fixed_point_area_artifact_is_mass_and_dmax_preserving_sphere() {
        let law = crate::P3PiecewiseParticleLaw::reconstruct(0.05, 400.0).unwrap();
        let diameter = law.graupel_to_partially_rimed_m * (1.0 + 1.0e-10);
        let particle = law.particle(diameter).unwrap();
        let raw_ratio = 4.0 * particle.projected_area_m2 / (PI * diameter.powi(2));
        assert_eq!(particle.region, P3ParticleRegion::PartiallyRimed);
        assert_eq!(
            particle.projected_area_consistency,
            P3ProjectedAreaConsistency::PinnedFinalCoefficientTransitionOvershoot
        );
        assert!((raw_ratio - 1.129_021_578_122_912_7).abs() < 1.0e-10);

        let source = P3QuadratureNode {
            maximum_dimension_m: diameter,
            number_concentration_m3: 1.0,
            particle,
        };
        assert!(projected_area_equivalent_oblate(source).is_err());
        let mapped = projected_area_equivalent_spheroid(source).unwrap();
        assert_eq!(mapped.habit(), PsdSpheroidHabit::Spherical);
        assert_eq!(
            mapped.minor_to_major_axis_ratio().to_bits(),
            1.0_f64.to_bits()
        );
        assert_eq!(mapped.equivolume_diameter_m().to_bits(), diameter.to_bits());
        assert!(mapped.fixed_point_area_closed());
        assert!((mapped.source_projected_area_ratio() - raw_ratio).abs() < 1.0e-14);
        assert!((mapped.bulk_density_kg_m3() - 310.780_824_238_289_7).abs() < 1.0e-6);
        assert!(mapped.bulk_density_kg_m3() <= P3_SOLID_ICE_DENSITY_KG_M3);
        let reconstructed_mass =
            mapped.bulk_density_kg_m3() * PI / 6.0 * mapped.equivolume_diameter_m().powi(3);
        assert!((reconstructed_mass - particle.mass_kg).abs() / particle.mass_kg < 1.0e-12);
        assert_eq!(
            mapped.source().particle.projected_area_m2,
            particle.projected_area_m2
        );
    }

    #[test]
    fn nominal_partially_rimed_area_remains_oblate_and_unchanged() {
        let law = crate::P3PiecewiseParticleLaw::reconstruct(0.5, 400.0).unwrap();
        let diameter = law.graupel_to_partially_rimed_m * 10.0;
        let particle = law.particle(diameter).unwrap();
        let raw_ratio = 4.0 * particle.projected_area_m2 / (PI * diameter.powi(2));
        assert_eq!(particle.region, P3ParticleRegion::PartiallyRimed);
        assert_eq!(
            particle.projected_area_consistency,
            P3ProjectedAreaConsistency::GeometricallyBounded
        );
        assert!(raw_ratio < 1.0);
        let mapped = projected_area_equivalent_spheroid(P3QuadratureNode {
            maximum_dimension_m: diameter,
            number_concentration_m3: 1.0,
            particle,
        })
        .unwrap();
        assert_eq!(mapped.habit(), PsdSpheroidHabit::Oblate);
        assert!(!mapped.fixed_point_area_closed());
        assert!((mapped.minor_to_major_axis_ratio() - raw_ratio).abs() < 1.0e-14);
        assert!((mapped.equivolume_diameter_m() - diameter * raw_ratio.cbrt()).abs() < 1.0e-14);
    }

    #[test]
    fn dense_unrimed_area_discontinuity_uses_mass_preserving_solid_ice_volume() {
        let law = crate::P3PiecewiseParticleLaw::reconstruct(0.0, 50.0).unwrap();
        let diameter = law.small_sphere_limit_m * (1.0 + 1.0e-8);
        let particle = law.particle(diameter).unwrap();
        assert_eq!(particle.region, P3ParticleRegion::DenseUnrimed);
        let source = P3QuadratureNode {
            maximum_dimension_m: diameter,
            number_concentration_m3: 1.0,
            particle,
        };
        let mapped = projected_area_equivalent_spheroid(source).unwrap();

        assert!(mapped.solid_ice_constrained());
        assert_eq!(
            mapped.bulk_density_kg_m3().to_bits(),
            P3_SOLID_ICE_DENSITY_KG_M3.to_bits()
        );
        let reconstructed_mass =
            mapped.bulk_density_kg_m3() * PI / 6.0 * mapped.equivolume_diameter_m().powi(3);
        assert!((reconstructed_mass - particle.mass_kg).abs() / particle.mass_kg < 1.0e-12);
        let raw_ratio = 4.0 * particle.projected_area_m2 / (PI * diameter.powi(2));
        assert!((mapped.minor_to_major_axis_ratio() - raw_ratio).abs() < 1.0e-12);
    }

    #[test]
    fn rayleigh_bridge_is_only_below_floor_for_exact_small_dense_spheres() {
        let law = crate::P3PiecewiseParticleLaw::reconstruct(0.0, 50.0).unwrap();
        let domain = PsdParticleDomain::new([50.0e-6, 0.089], [1.5, 917.0], [0.1, 1.0]).unwrap();
        let support = crate::PsdParticleSupport::uniform(domain);
        let config = P3TMatrixIntegrationConfig::new(
            P3TMatrixShapePolicy::ProjectedAreaEquivalentSpheroidGaussian20ResearchV1,
            P3SmallSphereScatteringPolicy::RayleighLimitBelowTableDiameterFloorV1,
            P3QuadratureConfig::default(),
            0.999,
            0.05,
            0.0025,
        )
        .unwrap();
        let make = |diameter| {
            let particle = law.particle(diameter).unwrap();
            let source = P3QuadratureNode {
                maximum_dimension_m: diameter,
                number_concentration_m3: 1.0,
                particle,
            };
            let mapped = projected_area_equivalent_spheroid(source).unwrap();
            (source, mapped)
        };
        let (below_source, below_mapped) = make(25.0e-6);
        assert_eq!(
            below_mapped.bulk_density_kg_m3().to_bits(),
            P3_SOLID_ICE_DENSITY_KG_M3.to_bits()
        );
        assert!(small_sphere_bridge_applies(
            config,
            below_source,
            below_mapped,
            support
        ));
        let disabled = P3TMatrixIntegrationConfig::new(
            P3TMatrixShapePolicy::ProjectedAreaEquivalentSpheroidGaussian20ResearchV1,
            P3SmallSphereScatteringPolicy::Disabled,
            P3QuadratureConfig::default(),
            0.999,
            0.05,
            0.0025,
        )
        .unwrap();
        assert!(!small_sphere_bridge_applies(
            disabled,
            below_source,
            below_mapped,
            support
        ));
        let mut wrong_density = below_mapped;
        wrong_density.bulk_density_kg_m3 = 1_000.0;
        assert!(!small_sphere_bridge_applies(
            config,
            below_source,
            wrong_density,
            support
        ));
        let (floor_source, floor_mapped) = make(50.0e-6);
        assert!(!small_sphere_bridge_applies(
            config,
            floor_source,
            floor_mapped,
            support
        ));
        let (nonspherical_source, nonspherical_mapped) = make(law.small_sphere_limit_m * 1.01);
        assert!(!small_sphere_bridge_applies(
            config,
            nonspherical_source,
            nonspherical_mapped,
            support
        ));
    }
}
