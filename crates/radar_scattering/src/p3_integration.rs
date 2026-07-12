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
    AdditiveScattering, OutputError, P3Psd, P3PsdError, P3QuadratureAudit, P3QuadratureConfig,
    P3QuadratureNode, PsdParticleDomain, PsdSpheroidHabit,
};

pub const P3_SPHERICAL_INTEGRATION_REVISION: &str =
    "wrf-p3-v5.4-shape-authoritative-spherical-integration-v1";
pub const P3_PROJECTED_AREA_EQUIVALENT_OBLATE_REVISION: &str =
    "wrf-p3-v5.4-projected-area-equivalent-oblate-gaussian20-research-v1";

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
    maximum_omitted_sixth_moment_fraction: f64,
}

impl P3TMatrixIntegrationConfig {
    pub fn new(
        shape_policy: P3TMatrixShapePolicy,
        quadrature: P3QuadratureConfig,
        maximum_omitted_number_fraction: f64,
        maximum_omitted_mass_fraction: f64,
        maximum_omitted_sixth_moment_fraction: f64,
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
                "maximum omitted P3 sixth-moment fraction",
                maximum_omitted_sixth_moment_fraction,
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
            maximum_omitted_sixth_moment_fraction,
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
    pub const fn maximum_omitted_sixth_moment_fraction(self) -> f64 {
        self.maximum_omitted_sixth_moment_fraction
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
pub struct P3TMatrixOmissionAudit {
    pub non_spherical_number_fraction: f64,
    pub non_spherical_mass_fraction: f64,
    pub non_spherical_sixth_moment_fraction: f64,
    pub outside_table_number_fraction: f64,
    pub outside_table_mass_fraction: f64,
    pub outside_table_sixth_moment_fraction: f64,
    pub total_omitted_number_fraction: f64,
    pub total_omitted_mass_fraction: f64,
    pub total_omitted_sixth_moment_fraction: f64,
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
    pub omission: P3TMatrixOmissionAudit,
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
        "P3 cannot be represented by the existing spheroidal tables without an invented shape: omitted number={number_fraction}, mass={mass_fraction}, D6={sixth_moment_fraction}; limits are {maximum_number}, {maximum_mass}, {maximum_sixth_moment}"
    )]
    ShapeOrTableOmission {
        number_fraction: f64,
        mass_fraction: f64,
        sixth_moment_fraction: f64,
        maximum_number: f64,
        maximum_mass: f64,
        maximum_sixth_moment: f64,
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
struct OmittedMoments {
    number: f64,
    mass: f64,
    sixth: f64,
    nodes: usize,
}

impl OmittedMoments {
    fn add(&mut self, node: &P3QuadratureNode) {
        self.number += node.number_concentration_m3;
        self.mass += node.number_concentration_m3 * node.particle.mass_kg;
        self.sixth += node.number_concentration_m3 * node.particle.maximum_dimension_m.powi(6);
        self.nodes += 1;
    }
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
    let quadrature = distribution
        .quadrature_with_dimension_breakpoints(config.quadrature, &diameter_breakpoints)
        .map_err(P3TMatrixIntegrationError::Psd)?;

    let mut non_spherical = OmittedMoments::default();
    let mut outside_table = OmittedMoments::default();
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
                projected_area_equivalent_nodes += 1;
                projected_area_equivalent_oblate(index, *node)?
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

    let closure = distribution.closure_audit();
    let total_number = closure.reconstructed_number_density_m3;
    let total_mass = closure.reconstructed_mass_concentration_kg_m3;
    let total_sixth = closure.reconstructed_sixth_moment_m3;
    let non_spherical_fractions = fractions(non_spherical, total_number, total_mass, total_sixth);
    let outside_table_fractions = fractions(outside_table, total_number, total_mass, total_sixth);
    let total_fractions = [
        non_spherical_fractions[0] + outside_table_fractions[0],
        non_spherical_fractions[1] + outside_table_fractions[1],
        non_spherical_fractions[2] + outside_table_fractions[2],
    ];
    if total_fractions[0] > config.maximum_omitted_number_fraction
        || total_fractions[1] > config.maximum_omitted_mass_fraction
        || total_fractions[2] > config.maximum_omitted_sixth_moment_fraction
    {
        return Err(P3TMatrixIntegrationError::ShapeOrTableOmission {
            number_fraction: total_fractions[0],
            mass_fraction: total_fractions[1],
            sixth_moment_fraction: total_fractions[2],
            maximum_number: config.maximum_omitted_number_fraction,
            maximum_mass: config.maximum_omitted_mass_fraction,
            maximum_sixth_moment: config.maximum_omitted_sixth_moment_fraction,
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
            omission: P3TMatrixOmissionAudit {
                non_spherical_number_fraction: non_spherical_fractions[0],
                non_spherical_mass_fraction: non_spherical_fractions[1],
                non_spherical_sixth_moment_fraction: non_spherical_fractions[2],
                outside_table_number_fraction: outside_table_fractions[0],
                outside_table_mass_fraction: outside_table_fractions[1],
                outside_table_sixth_moment_fraction: outside_table_fractions[2],
                total_omitted_number_fraction: total_fractions[0],
                total_omitted_mass_fraction: total_fractions[1],
                total_omitted_sixth_moment_fraction: total_fractions[2],
            },
        },
    })
}

fn projected_area_equivalent_oblate<E: Error + 'static>(
    node_index: usize,
    node: P3QuadratureNode,
) -> Result<P3TMatrixParticleNode, P3TMatrixIntegrationError<E>> {
    let maximum_dimension = node.particle.maximum_dimension_m;
    let raw_ratio = 4.0 * node.particle.projected_area_m2 / (PI * maximum_dimension.powi(2));
    if !raw_ratio.is_finite() || raw_ratio <= 0.0 || raw_ratio > 1.0 + 1.0e-10 {
        return Err(P3TMatrixIntegrationError::InvalidEquivalentGeometry {
            node_index,
            message: format!("4A/(pi Dmax^2) must be within (0, 1], got {raw_ratio}"),
        });
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
        return Err(P3TMatrixIntegrationError::InvalidEquivalentGeometry {
            node_index,
            message: format!(
                "derived Deq and density must be finite and positive, got {equivolume_diameter} m and {density} kg m-3"
            ),
        });
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
    omitted: OmittedMoments,
    total_number: f64,
    total_mass: f64,
    total_sixth: f64,
) -> [f64; 3] {
    [
        omitted.number / total_number,
        omitted.mass / total_mass,
        omitted.sixth / total_sixth,
    ]
}

fn contains(range: [f64; 2], value: f64) -> bool {
    value >= range[0] && value <= range[1]
}
