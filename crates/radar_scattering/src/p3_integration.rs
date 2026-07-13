//! Fail-closed integration of the exact P3 PSD over shape-authoritative nodes.
//!
//! P3 predicts maximum dimension, mass, and projected area, but it does not
//! predict a unique spheroidal axis ratio or canting distribution for its
//! dense-unrimed and partially-rimed regions. Strict mode therefore evaluates
//! only exact spheres and audits every other particle as omitted. A separate,
//! unmistakably research-only mode derives one projected-area-equivalent
//! oblate from the native mass/area law; it never labels that shape as P3
//! truth. Both modes reject table-domain omissions above declared budgets.

use std::error::Error;
use std::f64::consts::PI;

use thiserror::Error;

use crate::{
    AdditiveScattering, OutputError, P3PopulationMoments, P3Psd, P3PsdError, P3QuadratureAudit,
    P3QuadratureConfig, P3QuadratureNode, PsdParticleDomain, PsdSpheroidHabit,
};

pub const P3_SPHERICAL_INTEGRATION_REVISION: &str =
    "wrf-p3-v5.4-shape-authoritative-spherical-integration-v4";
pub const P3_PROJECTED_AREA_EQUIVALENT_OBLATE_REVISION: &str =
    "wrf-p3-v5.4-projected-area-equivalent-oblate-gaussian20-research-v4";

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
}

/// Explicit omission gates for the only shape-authoritative P3/T-matrix seam.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct P3TMatrixIntegrationConfig {
    shape_policy: P3TMatrixShapePolicy,
    quadrature: P3QuadratureConfig,
    maximum_omitted_number_fraction: f64,
    maximum_omitted_mass_fraction: f64,
    maximum_omitted_radar_weight_fraction: f64,
}

impl P3TMatrixIntegrationConfig {
    pub fn new(
        shape_policy: P3TMatrixShapePolicy,
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
        "P3 cannot be represented inside the inferred WRF source integration domain by the existing spheroidal tables without an invented shape: in-source omitted number={number_fraction}, mass={mass_fraction}, equivalent-ice-volume-squared radar weight={radar_weight_fraction}; in-source limits are {maximum_number}, {maximum_mass}, {maximum_radar_weight}"
    )]
    ShapeOrTableOmission {
        number_fraction: f64,
        mass_fraction: f64,
        radar_weight_fraction: f64,
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
    for (index, node) in quadrature.nodes.iter().enumerate() {
        let table_node = match config.shape_policy {
            P3TMatrixShapePolicy::StrictShapeAuthoritativeSpheres => {
                if !node.particle.is_exact_sphere() {
                    non_spherical.add(node);
                    continue;
                }
                P3TMatrixParticleNode {
                    source: *node,
                    equivolume_diameter_m: node.particle.maximum_dimension_m,
                    bulk_density_kg_m3: node.particle.effective_spherical_density_kg_m3,
                    minor_to_major_axis_ratio: 1.0,
                    habit: PsdSpheroidHabit::Spherical,
                    shape_policy: config.shape_policy,
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
        };
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
            },
            config,
            quadrature: quadrature.audit,
            supported_nodes: supported.len(),
            non_spherical_nodes: non_spherical.nodes,
            outside_table_nodes: outside_table.nodes,
            projected_area_equivalent_nodes,
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

fn projected_area_equivalent_oblate(
    node: P3QuadratureNode,
) -> Result<P3TMatrixParticleNode, &'static str> {
    let maximum_dimension = node.particle.maximum_dimension_m;
    let raw_ratio = 4.0 * node.particle.projected_area_m2 / (PI * maximum_dimension.powi(2));
    if !raw_ratio.is_finite() || raw_ratio <= 0.0 || raw_ratio > 1.0 + 1.0e-10 {
        return Err("4A/(pi Dmax^2) is outside the equivalent-oblate domain");
    }
    // Only absorb roundoff at the analytic spherical boundary; values above
    // the tolerance fail instead of silently becoming spheres.
    let ratio = raw_ratio.min(1.0);
    let equivolume_diameter = maximum_dimension * ratio.cbrt();
    let volume = PI / 6.0 * equivolume_diameter.powi(3);
    let density = node.particle.mass_kg / volume;
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
        minor_to_major_axis_ratio: ratio,
        habit: if (ratio - 1.0).abs() <= 1.0e-10 {
            PsdSpheroidHabit::Spherical
        } else {
            PsdSpheroidHabit::Oblate
        },
        shape_policy: P3TMatrixShapePolicy::ProjectedAreaEquivalentOblateGaussian20ResearchV1,
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
    use crate::{P3ParticleGeometry, P3ParticleRegion, P3ShapeAuthority};

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
    fn projected_area_above_circumscribing_circle_is_not_clamped() {
        let mut invalid = node(0.01, 1.0e-6, 1.0);
        invalid.particle.projected_area_m2 *= 1.001;
        assert!(projected_area_equivalent_oblate(invalid).is_err());
    }
}
