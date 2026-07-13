use radar_scattering::{
    AdditiveScattering, PreparedInterpolationPlan, PreparedTMatrixLutInterpolation,
    ResearchTMatrixLut, Sha256Digest,
};
use thiserror::Error;

use crate::{
    CudaDeviceInfo,
    kernel::{
        CUDA_MAX_ACTIVE_AXES, CudaLutExecutor, CudaLutNodePlan, CudaLutSegment,
        CudaSegmentExecutionError,
    },
};

/// Opaque table-admitted node ready for deterministic CUDA staging.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CudaPreparedTMatrixNode {
    table_file_sha256: Sha256Digest,
    plan: CudaLutNodePlan,
}

impl CudaPreparedTMatrixNode {
    /// Convert table-admitted T-matrix execution facts into the fixed CUDA
    /// staging layout and attach one PSD population weight.
    ///
    /// The interpolation plan and terminal speed arrive together in an opaque
    /// [`PreparedTMatrixLutInterpolation`]. This deliberately does not expose
    /// a public constructor accepting a naked plan or caller-supplied speed,
    /// which would bypass the owning table's identity gate.
    pub fn new(
        prepared: &PreparedTMatrixLutInterpolation,
        number_concentration_m3: f64,
    ) -> Result<Self, CudaLutNodePreparationError> {
        let plan = convert_fixed_plan(
            prepared.interpolation_plan(),
            prepared.positive_down_fall_speed_m_s(),
            number_concentration_m3,
        )?;
        Ok(Self {
            table_file_sha256: prepared.table_file_sha256(),
            plan,
        })
    }
}

/// One ordered output segment in a staged CUDA node array.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CudaTMatrixSegment {
    first_node: u32,
    node_count: u32,
}

impl CudaTMatrixSegment {
    pub fn new(first_node: usize, node_count: usize) -> Result<Self, CudaSegmentLayoutError> {
        Ok(Self {
            first_node: u32::try_from(first_node)
                .map_err(|_| CudaSegmentLayoutError::IndexRange)?,
            node_count: u32::try_from(node_count)
                .map_err(|_| CudaSegmentLayoutError::IndexRange)?,
        })
    }

    #[must_use]
    pub const fn first_node(self) -> u32 {
        self.first_node
    }

    #[must_use]
    pub const fn node_count(self) -> u32 {
        self.node_count
    }
}

/// Opaque handle proving that an exact validated LUT payload was uploaded by
/// this API. It carries no host table allocation and can move with a dedicated
/// batching worker after preload completes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CudaPreloadedTMatrixTable {
    table_file_sha256: Sha256Digest,
    point_count: usize,
}

impl CudaPreloadedTMatrixTable {
    #[must_use]
    pub const fn table_file_sha256(self) -> Sha256Digest {
        self.table_file_sha256
    }

    #[must_use]
    pub const fn point_count(self) -> usize {
        self.point_count
    }
}

/// Identity-safe, persistent-LUT CUDA executor. Only table-owned prepared
/// descriptors enter this API; the raw plan, terminal speed, and table payload
/// cannot be replaced independently by callers.
pub struct CudaTMatrixExecutor {
    inner: CudaLutExecutor,
}

impl std::fmt::Debug for CudaTMatrixExecutor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CudaTMatrixExecutor")
            .field("device", self.inner.device())
            .field("kernel_artifact", &self.inner.kernel_artifact())
            .finish_non_exhaustive()
    }
}

impl CudaTMatrixExecutor {
    pub fn open(ordinal: usize) -> Result<Self, CudaTMatrixExecutionError> {
        Ok(Self {
            inner: CudaLutExecutor::open(ordinal)?,
        })
    }

    #[must_use]
    pub const fn device(&self) -> &CudaDeviceInfo {
        self.inner.device()
    }

    #[must_use]
    pub const fn kernel_artifact(&self) -> &'static str {
        self.inner.kernel_artifact()
    }

    pub fn preload_table(
        &mut self,
        table: &ResearchTMatrixLut,
    ) -> Result<CudaPreloadedTMatrixTable, CudaTMatrixExecutionError> {
        let table_file_sha256 = table.file_sha256();
        let point_count = self
            .inner
            .preload_lut(table_file_sha256, table.offline_lut().values())?;
        Ok(CudaPreloadedTMatrixTable {
            table_file_sha256,
            point_count,
        })
    }

    pub fn evaluate_segments(
        &mut self,
        table: &ResearchTMatrixLut,
        nodes: &[CudaPreparedTMatrixNode],
        segments: &[CudaTMatrixSegment],
    ) -> Result<Vec<AdditiveScattering>, CudaTMatrixExecutionError> {
        let preloaded = self.preload_table(table)?;
        self.evaluate_preloaded_segments(preloaded, nodes, segments)
    }

    pub fn evaluate_preloaded_segments(
        &mut self,
        table: CudaPreloadedTMatrixTable,
        nodes: &[CudaPreparedTMatrixNode],
        segments: &[CudaTMatrixSegment],
    ) -> Result<Vec<AdditiveScattering>, CudaTMatrixExecutionError> {
        validate_node_table_identity(table.table_file_sha256, nodes)?;
        let plans = nodes.iter().map(|node| node.plan).collect::<Vec<_>>();
        let segments = segments
            .iter()
            .map(|segment| CudaLutSegment {
                first_node: segment.first_node,
                node_count: segment.node_count,
            })
            .collect::<Vec<_>>();
        self.inner
            .evaluate_preloaded_segments(
                table.table_file_sha256,
                table.point_count,
                &plans,
                &segments,
            )
            .map_err(Into::into)
    }
}

fn validate_node_table_identity(
    expected: Sha256Digest,
    nodes: &[CudaPreparedTMatrixNode],
) -> Result<(), CudaTMatrixExecutionError> {
    if let Some((node, actual)) = nodes.iter().enumerate().find_map(|(node, prepared)| {
        (prepared.table_file_sha256 != expected).then_some((node, prepared.table_file_sha256))
    }) {
        Err(CudaTMatrixExecutionError::TableIdentity {
            node,
            expected,
            actual,
        })
    } else {
        Ok(())
    }
}

fn convert_fixed_plan(
    plan: &PreparedInterpolationPlan,
    positive_down_fall_speed_m_s: f64,
    number_concentration_m3: f64,
) -> Result<CudaLutNodePlan, CudaLutNodePreparationError> {
    if !number_concentration_m3.is_finite() || number_concentration_m3 < 0.0 {
        return Err(CudaLutNodePreparationError::InvalidNumberConcentration {
            value: number_concentration_m3,
        });
    }
    let active_axis_count = plan.active_axis_count() as usize;
    if active_axis_count > CUDA_MAX_ACTIVE_AXES {
        return Err(CudaLutNodePreparationError::ActiveAxisCount {
            actual: active_axis_count,
            maximum: CUDA_MAX_ACTIVE_AXES,
        });
    }

    let mut upper_point_offsets = [0_u64; CUDA_MAX_ACTIVE_AXES];
    upper_point_offsets[..active_axis_count]
        .copy_from_slice(&plan.upper_point_offsets()[..active_axis_count]);
    let mut upper_fractions = [0.0_f64; CUDA_MAX_ACTIVE_AXES];
    upper_fractions[..active_axis_count]
        .copy_from_slice(&plan.upper_fractions()[..active_axis_count]);

    Ok(CudaLutNodePlan {
        base_point_index: plan.base_point_index(),
        upper_point_offsets,
        upper_fractions,
        active_axis_count: plan.active_axis_count(),
        number_concentration_m3,
        positive_down_fall_speed_m_s,
    })
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum CudaLutNodePreparationError {
    #[error("prepared LUT node has {actual} active axes; CUDA layout supports {maximum}")]
    ActiveAxisCount { actual: usize, maximum: usize },
    #[error("CUDA LUT node number concentration is invalid: {value} m^-3")]
    InvalidNumberConcentration { value: f64 },
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum CudaSegmentLayoutError {
    #[error("CUDA segment index/count exceeds the 32-bit kernel ABI")]
    IndexRange,
}

#[derive(Debug, Error)]
pub enum CudaTMatrixExecutionError {
    #[error("CUDA node {node} belongs to LUT {actual}, but execution table is {expected}")]
    TableIdentity {
        node: usize,
        expected: Sha256Digest,
        actual: Sha256Digest,
    },
    #[error(transparent)]
    Kernel(#[from] CudaSegmentExecutionError),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use radar_scattering::{
        AdditiveScattering, Axis, AxisCoordinate, AxisKind, GeneratorMetadata, KernelModel,
        MeltingModel, OfflineLut, OrientationModel, ScienceMetadata, TableValidation,
        TemporalSampling, Unit,
    };

    use super::*;

    fn axis_unit(kind: AxisKind) -> Unit {
        match kind {
            AxisKind::EquivolumeDiameter => Unit::Meter,
            AxisKind::Temperature => Unit::Kelvin,
            AxisKind::BulkDensity | AxisKind::RimeDensity => Unit::KilogramPerCubicMeter,
            AxisKind::CondensedVolumeFraction
            | AxisKind::LiquidMassFraction
            | AxisKind::MinorToMajorAxisRatio
            | AxisKind::RimeMassFraction => Unit::UnitlessFraction,
            AxisKind::Frequency => Unit::Hertz,
            AxisKind::RadarElevation | AxisKind::CantingAngle => Unit::Degree,
            AxisKind::TimeOffset => Unit::Second,
        }
    }

    fn fixed_plan(kinds: &[AxisKind]) -> PreparedInterpolationPlan {
        let axes = kinds
            .iter()
            .copied()
            .map(|kind| Axis::new(kind, axis_unit(kind), vec![0.0, 2.0]).unwrap())
            .collect::<Vec<_>>();
        let point =
            AdditiveScattering::from_components([1.0, 1.0, 0.5, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0])
                .unwrap();
        let generator = GeneratorMetadata::new(
            "simradar-cuda-adapter-test",
            "1",
            "unit-test",
            "unit-test",
            None,
            BTreeMap::new(),
        )
        .unwrap();
        let science = ScienceMetadata::new(
            KernelModel::SyntheticFixtureOnly,
            OrientationModel::FixedEuler {
                yaw_deg: 0.0,
                pitch_deg: 0.0,
                roll_deg: 0.0,
            },
            MeltingModel::Dry,
            TemporalSampling::Instantaneous,
            TableValidation::SyntheticFixtureOnly,
        )
        .unwrap();
        let table = OfflineLut::new(
            axes,
            generator,
            r#"{"software_test_only":true}"#,
            science,
            vec![point; 1_usize << kinds.len()],
        )
        .unwrap();
        table
            .prepare_interpolation(
                &kinds
                    .iter()
                    .copied()
                    .map(|kind| AxisCoordinate::new(kind, 0.5).unwrap())
                    .collect::<Vec<_>>(),
            )
            .unwrap()
    }

    #[test]
    fn conversion_preserves_compacted_axis_order_and_zero_fills_tail() {
        let plan = fixed_plan(&[
            AxisKind::EquivolumeDiameter,
            AxisKind::Temperature,
            AxisKind::BulkDensity,
        ]);
        let converted = convert_fixed_plan(&plan, 3.25, 12.5).unwrap();
        assert_eq!(converted.base_point_index, 0);
        assert_eq!(converted.active_axis_count, 3);
        assert_eq!(&converted.upper_point_offsets[..3], &[4, 2, 1]);
        assert_eq!(&converted.upper_fractions[..3], &[0.25, 0.25, 0.25]);
        assert!(
            converted.upper_point_offsets[3..]
                .iter()
                .all(|value| *value == 0)
        );
        assert!(
            converted.upper_fractions[3..]
                .iter()
                .all(|value| *value == 0.0)
        );
        assert_eq!(converted.number_concentration_m3, 12.5);
        assert_eq!(converted.positive_down_fall_speed_m_s, 3.25);
    }

    #[test]
    fn conversion_rejects_layout_wider_than_kernel_without_truncation() {
        let plan = fixed_plan(&[
            AxisKind::EquivolumeDiameter,
            AxisKind::Temperature,
            AxisKind::BulkDensity,
            AxisKind::CondensedVolumeFraction,
            AxisKind::LiquidMassFraction,
            AxisKind::MinorToMajorAxisRatio,
            AxisKind::Frequency,
            AxisKind::RadarElevation,
            AxisKind::CantingAngle,
        ]);
        assert_eq!(plan.active_axis_count(), 9);
        assert_eq!(
            convert_fixed_plan(&plan, 1.0, 1.0),
            Err(CudaLutNodePreparationError::ActiveAxisCount {
                actual: 9,
                maximum: CUDA_MAX_ACTIVE_AXES,
            })
        );
    }

    #[test]
    fn conversion_rejects_nonfinite_or_negative_population_weight() {
        let plan = fixed_plan(&[AxisKind::EquivolumeDiameter]);
        for value in [f64::NAN, f64::INFINITY, -1.0] {
            assert!(matches!(
                convert_fixed_plan(&plan, 1.0, value),
                Err(CudaLutNodePreparationError::InvalidNumberConcentration { .. })
            ));
        }
    }
}
