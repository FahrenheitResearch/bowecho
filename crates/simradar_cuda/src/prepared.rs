use radar_scattering::{PreparedInterpolationPlan, PreparedTMatrixLutInterpolation, Sha256Digest};
use thiserror::Error;

pub(crate) const CUDA_MAX_ACTIVE_AXES: usize = 8;

/// Host-side form of one CPU-admitted LUT interpolation plus the population
/// scale/fall-speed facts applied after interpolation.
///
/// This fixed layout is portable and contains no driver-owned state. The CUDA
/// kernel module consumes it on supported platforms, while every platform can
/// still prepare identical batches through [`CudaPreparedTMatrixNode`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CudaLutNodePlan {
    pub(crate) base_point_index: u64,
    pub(crate) upper_point_offsets: [u64; CUDA_MAX_ACTIVE_AXES],
    pub(crate) upper_fractions: [f64; CUDA_MAX_ACTIVE_AXES],
    pub(crate) active_axis_count: u32,
    pub(crate) number_concentration_m3: f64,
    pub(crate) positive_down_fall_speed_m_s: f64,
}

/// Opaque table-admitted node ready for deterministic CUDA staging.
///
/// This descriptor is available on every BowEcho target. Constructing it does
/// not load or link CUDA; it only copies the table-owned, CPU-validated fixed
/// interpolation plan into the common batch ABI.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CudaPreparedTMatrixNode {
    pub(crate) table_file_sha256: Sha256Digest,
    pub(crate) plan: CudaLutNodePlan,
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
