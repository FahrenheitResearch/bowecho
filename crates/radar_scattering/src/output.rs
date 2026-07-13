use thiserror::Error;

const RELATIVE_INVARIANT_TOLERANCE: f64 = 64.0 * f64::EPSILON;
const ACCUMULATOR_MAX_LINEAR_Z: f64 = 1.0e15;
const ACCUMULATOR_MAX_KDP_DEG_KM: f64 = 1_000.0;
const ACCUMULATOR_MAX_ATTENUATION_DB_KM: f64 = 100.0;
const ACCUMULATOR_MAX_FALL_SPEED_MPS: f64 = 50.0;
const ACCUMULATOR_MAX_FALL_VARIANCE_M2S2: f64 = 2_500.0;

/// Linear equivalent reflectivity factor in mm^6 m^-3.
#[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd)]
pub struct LinearReflectivity(f64);

impl LinearReflectivity {
    pub fn new(value_mm6_m3: f64) -> Result<Self, OutputError> {
        finite_nonnegative("linear reflectivity", value_mm6_m3).map(Self)
    }

    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

/// Complex copolar HH/VV covariance in linear reflectivity units.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ComplexCovariance {
    re: f64,
    im: f64,
}

impl ComplexCovariance {
    pub fn new(re: f64, im: f64) -> Result<Self, OutputError> {
        finite("covariance real component", re)?;
        finite("covariance imaginary component", im)?;
        Ok(Self { re, im })
    }

    #[must_use]
    pub const fn re(self) -> f64 {
        self.re
    }

    #[must_use]
    pub const fn im(self) -> f64 {
        self.im
    }

    #[must_use]
    pub fn magnitude(self) -> f64 {
        self.re.hypot(self.im)
    }
}

/// Specific differential phase in degrees per kilometre.
///
/// This type permits a finite negative value because vertically preferred
/// orientation can reverse the sign. The application accumulator preserves
/// signed KDP within its symmetric representable range.
#[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd)]
pub struct SpecificDifferentialPhase(f64);

impl SpecificDifferentialPhase {
    pub fn new(value_deg_km: f64) -> Result<Self, OutputError> {
        finite("specific differential phase", value_deg_km).map(Self)
    }

    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

/// One-way specific attenuation in dB per kilometre.
#[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd)]
pub struct SpecificAttenuation(f64);

impl SpecificAttenuation {
    pub fn new(value_db_km: f64) -> Result<Self, OutputError> {
        finite_nonnegative("specific attenuation", value_db_km).map(Self)
    }

    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

/// Additive ZH-weighted terminal-fall-speed moments.
///
/// `first` has units `(mm^6 m^-3)(m s^-1)` and `second` has units
/// `(mm^6 m^-3)(m^2 s^-2)`. ZH is the zeroth-moment weight.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FallSpeedMoments {
    first: f64,
    second: f64,
}

impl FallSpeedMoments {
    pub fn new(first: f64, second: f64) -> Result<Self, OutputError> {
        finite_nonnegative("fall-speed first moment", first)?;
        finite_nonnegative("fall-speed second moment", second)?;
        Ok(Self { first, second })
    }

    pub fn from_mean_variance(
        zh: LinearReflectivity,
        mean_mps: f64,
        variance_m2s2: f64,
    ) -> Result<Self, OutputError> {
        finite_nonnegative("fall-speed mean", mean_mps)?;
        finite_nonnegative("fall-speed variance", variance_m2s2)?;
        let first = zh.get() * mean_mps;
        let second = zh.get() * (variance_m2s2 + mean_mps * mean_mps);
        Self::new(first, second)
    }

    #[must_use]
    pub const fn first(self) -> f64 {
        self.first
    }

    #[must_use]
    pub const fn second(self) -> f64 {
        self.second
    }

    fn mean_variance(self, zh: LinearReflectivity) -> Result<(f64, f64), OutputError> {
        if zh.get() == 0.0 {
            if self.first == 0.0 && self.second == 0.0 {
                return Ok((0.0, 0.0));
            }
            return Err(OutputError::FallMomentsWithoutReflectivity);
        }

        let mean = self.first / zh.get();
        let raw_second = self.second / zh.get();
        finite("fall-speed mean derived from moments", mean)?;
        finite("fall-speed raw second moment", raw_second)?;
        let variance = raw_second - mean * mean;
        let tolerance = RELATIVE_INVARIANT_TOLERANCE * raw_second.abs().max(1.0);
        if variance < -tolerance
            && !subnormal_moment_intervals_admit_nonnegative_variance(
                self.first,
                self.second,
                zh.get(),
            )
        {
            return Err(OutputError::NegativeFallSpeedVariance {
                first: self.first,
                second: self.second,
                zh: zh.get(),
            });
        }
        Ok((mean, variance.max(0.0)))
    }
}

fn subnormal_moment_intervals_admit_nonnegative_variance(first: f64, second: f64, zh: f64) -> bool {
    // Scaling a valid node by an extremely small concentration independently
    // rounds ZH, ZH*V, and ZH*V^2 onto the fixed f64-subnormal lattice. Their
    // ratios can then imply a small negative variance even though the exact
    // moments were valid. Do not raise the normal tolerance or discard other
    // scattering components: accept only when the three half-ULP intervals
    // contain at least one valid moment triple, S*Z >= F^2.
    let quanta = |value: f64| {
        if value == 0.0 {
            Some(0_u128)
        } else if value.is_sign_positive() && value.is_subnormal() {
            Some(u128::from(value.to_bits()))
        } else {
            None
        }
    };
    let (Some(first), Some(second), Some(zh)) = (quanta(first), quanta(second), quanta(zh)) else {
        return false;
    };

    let second_upper_twice = 2 * second + 1;
    let zh_upper_twice = 2 * zh + 1;
    let first_lower_twice = (2 * first).saturating_sub(1);
    second_upper_twice * zh_upper_twice >= first_lower_twice * first_lower_twice
}

/// A contribution that is additive in every stored component.
///
/// ZDR, rhoHV, mean terminal speed, and speed variance are intentionally not
/// stored because they are nonlinear derived products.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AdditiveScattering {
    zh: LinearReflectivity,
    zv: LinearReflectivity,
    covariance: ComplexCovariance,
    kdp: SpecificDifferentialPhase,
    ah: SpecificAttenuation,
    av: SpecificAttenuation,
    fall_speed: FallSpeedMoments,
}

impl AdditiveScattering {
    pub const COMPONENT_COUNT: usize = 9;

    pub fn new(
        zh: LinearReflectivity,
        zv: LinearReflectivity,
        covariance: ComplexCovariance,
        kdp: SpecificDifferentialPhase,
        ah: SpecificAttenuation,
        av: SpecificAttenuation,
        fall_speed: FallSpeedMoments,
    ) -> Result<Self, OutputError> {
        let candidate = Self {
            zh,
            zv,
            covariance,
            kdp,
            ah,
            av,
            fall_speed,
        };
        candidate.validate()?;
        Ok(candidate)
    }

    pub fn from_components(values: [f64; Self::COMPONENT_COUNT]) -> Result<Self, OutputError> {
        Self::new(
            LinearReflectivity::new(values[0])?,
            LinearReflectivity::new(values[1])?,
            ComplexCovariance::new(values[2], values[3])?,
            SpecificDifferentialPhase::new(values[4])?,
            SpecificAttenuation::new(values[5])?,
            SpecificAttenuation::new(values[6])?,
            FallSpeedMoments::new(values[7], values[8])?,
        )
    }

    #[must_use]
    pub fn components(self) -> [f64; Self::COMPONENT_COUNT] {
        [
            self.zh.get(),
            self.zv.get(),
            self.covariance.re(),
            self.covariance.im(),
            self.kdp.get(),
            self.ah.get(),
            self.av.get(),
            self.fall_speed.first(),
            self.fall_speed.second(),
        ]
    }

    #[must_use]
    pub const fn zh(self) -> LinearReflectivity {
        self.zh
    }

    #[must_use]
    pub const fn zv(self) -> LinearReflectivity {
        self.zv
    }

    #[must_use]
    pub const fn covariance(self) -> ComplexCovariance {
        self.covariance
    }

    #[must_use]
    pub const fn kdp(self) -> SpecificDifferentialPhase {
        self.kdp
    }

    #[must_use]
    pub const fn ah(self) -> SpecificAttenuation {
        self.ah
    }

    #[must_use]
    pub const fn av(self) -> SpecificAttenuation {
        self.av
    }

    #[must_use]
    pub const fn fall_speed(self) -> FallSpeedMoments {
        self.fall_speed
    }

    pub fn checked_scale(self, nonnegative_weight: f64) -> Result<Self, OutputError> {
        finite_nonnegative("additive contribution weight", nonnegative_weight)?;
        let mut values = self.components();
        for value in &mut values {
            *value *= nonnegative_weight;
        }
        // At the extreme low-concentration tail of a quadrature, binary64 can
        // round ZH to zero while the larger ZH*V and ZH*V^2 products remain as
        // subnormals. Those moments no longer have a representable zeroth-
        // moment weight and cannot contribute a derived fall speed. Collapse
        // only that scale-underflow case; `self` was already validated, so an
        // evaluator cannot use this path to hide intrinsically inconsistent
        // nonzero moments paired with zero ZH.
        if self.zh.get() > 0.0 && values[0] == 0.0 {
            values[7] = 0.0;
            values[8] = 0.0;
        }
        Self::from_components(values)
    }

    pub fn checked_add(self, other: Self) -> Result<Self, OutputError> {
        let left = self.components();
        let right = other.components();
        let mut sum = [0.0; Self::COMPONENT_COUNT];
        for index in 0..Self::COMPONENT_COUNT {
            sum[index] = left[index] + right[index];
        }
        Self::from_components(sum)
    }

    /// Convert to the field-for-field quantity shape expected by the current
    /// application `PolarAccumulator::add` seam.
    ///
    /// The conversion is deliberately fallible: the existing accumulator
    /// accepts only positive-down fall speed and stores f32. Signed KDP is
    /// accepted over the accumulator's symmetric range. No clamping or
    /// saturation occurs here.
    pub fn to_polar_accumulator_quantities(
        self,
    ) -> Result<PolarAccumulatorQuantities, OutputError> {
        let (fall_speed_mps, fall_speed_variance_m2s2) = self.fall_speed.mean_variance(self.zh)?;
        accumulator_range("ZH", self.zh.get(), 0.0, ACCUMULATOR_MAX_LINEAR_Z)?;
        accumulator_range("ZV", self.zv.get(), 0.0, ACCUMULATOR_MAX_LINEAR_Z)?;
        accumulator_range(
            "covariance magnitude",
            self.covariance.magnitude(),
            0.0,
            ACCUMULATOR_MAX_LINEAR_Z,
        )?;
        accumulator_range(
            "KDP",
            self.kdp.get(),
            -ACCUMULATOR_MAX_KDP_DEG_KM,
            ACCUMULATOR_MAX_KDP_DEG_KM,
        )?;
        accumulator_range("AH", self.ah.get(), 0.0, ACCUMULATOR_MAX_ATTENUATION_DB_KM)?;
        accumulator_range("AV", self.av.get(), 0.0, ACCUMULATOR_MAX_ATTENUATION_DB_KM)?;
        accumulator_range(
            "fall-speed mean",
            fall_speed_mps,
            0.0,
            ACCUMULATOR_MAX_FALL_SPEED_MPS,
        )?;
        accumulator_range(
            "fall-speed variance",
            fall_speed_variance_m2s2,
            0.0,
            ACCUMULATOR_MAX_FALL_VARIANCE_M2S2,
        )?;
        if self.zh.get() > 0.0 && self.zv.get() > 0.0 {
            accumulator_range(
                "derived ZDR",
                10.0 * (self.zh.get() / self.zv.get()).log10(),
                -20.0,
                20.0,
            )?;
        }
        Ok(PolarAccumulatorQuantities {
            zh: to_f32("ZH", self.zh.get())?,
            zv: to_f32("ZV", self.zv.get())?,
            cov_re: to_f32("covariance real component", self.covariance.re())?,
            cov_im: to_f32("covariance imaginary component", self.covariance.im())?,
            kdp_deg_km: to_f32("KDP", self.kdp.get())?,
            ah_db_km: to_f32("AH", self.ah.get())?,
            av_db_km: to_f32("AV", self.av.get())?,
            fall_speed_mps: to_f32("fall-speed mean", fall_speed_mps)?,
            fall_speed_variance_m2s2: to_f32("fall-speed variance", fall_speed_variance_m2s2)?,
        })
    }

    fn validate(self) -> Result<(), OutputError> {
        let covariance_limit = self.zh.get().sqrt() * self.zv.get().sqrt();
        let covariance = self.covariance.magnitude();
        let tolerance = RELATIVE_INVARIANT_TOLERANCE * covariance_limit.max(1.0);
        if covariance > covariance_limit + tolerance {
            return Err(OutputError::CovarianceBound {
                magnitude: covariance,
                maximum: covariance_limit,
            });
        }
        self.fall_speed.mean_variance(self.zh)?;
        Ok(())
    }
}

/// Exact f32 field shape consumed by the current application accumulator.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PolarAccumulatorQuantities {
    pub zh: f32,
    pub zv: f32,
    pub cov_re: f32,
    pub cov_im: f32,
    pub kdp_deg_km: f32,
    pub ah_db_km: f32,
    pub av_db_km: f32,
    pub fall_speed_mps: f32,
    pub fall_speed_variance_m2s2: f32,
}

fn finite(field: &'static str, value: f64) -> Result<f64, OutputError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(OutputError::NonFinite { field, value })
    }
}

fn finite_nonnegative(field: &'static str, value: f64) -> Result<f64, OutputError> {
    finite(field, value)?;
    if value >= 0.0 {
        Ok(value)
    } else {
        Err(OutputError::Negative { field, value })
    }
}

fn to_f32(field: &'static str, value: f64) -> Result<f32, OutputError> {
    if value < -(f32::MAX as f64) || value > f32::MAX as f64 {
        return Err(OutputError::F32Range { field, value });
    }
    let converted = value as f32;
    if converted.is_finite() {
        Ok(converted)
    } else {
        Err(OutputError::F32Range { field, value })
    }
}

fn accumulator_range(
    field: &'static str,
    value: f64,
    minimum: f64,
    maximum: f64,
) -> Result<(), OutputError> {
    if (minimum..=maximum).contains(&value) {
        Ok(())
    } else {
        Err(OutputError::PolarAccumulatorRange {
            field,
            value,
            minimum,
            maximum,
        })
    }
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum OutputError {
    #[error("{field} must be finite, got {value}")]
    NonFinite { field: &'static str, value: f64 },
    #[error("{field} must be nonnegative, got {value}")]
    Negative { field: &'static str, value: f64 },
    #[error("HH/VV covariance magnitude {magnitude} exceeds sqrt(ZH*ZV) {maximum}")]
    CovarianceBound { magnitude: f64, maximum: f64 },
    #[error("nonzero fall-speed moments require positive ZH")]
    FallMomentsWithoutReflectivity,
    #[error("fall-speed moments imply negative variance (first={first}, second={second}, ZH={zh})")]
    NegativeFallSpeedVariance { first: f64, second: f64, zh: f64 },
    #[deprecated(note = "signed KDP is representable by the application accumulator")]
    #[error("negative KDP {value} cannot be passed to the legacy PolarAccumulator seam")]
    NegativeKdpNotRepresentable { value: f64 },
    #[error("{field} value {value} is outside finite f32 range")]
    F32Range { field: &'static str, value: f64 },
    #[error(
        "{field} value {value} is outside the current PolarAccumulator range [{minimum}, {maximum}]"
    )]
    PolarAccumulatorRange {
        field: &'static str,
        value: f64,
        minimum: f64,
        maximum: f64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(zh: f64, mean: f64, variance: f64) -> AdditiveScattering {
        let zh = LinearReflectivity::new(zh).unwrap();
        AdditiveScattering::new(
            zh,
            LinearReflectivity::new(0.8 * zh.get()).unwrap(),
            ComplexCovariance::new(0.5 * zh.get(), -0.1 * zh.get()).unwrap(),
            SpecificDifferentialPhase::new(0.7).unwrap(),
            SpecificAttenuation::new(0.03).unwrap(),
            SpecificAttenuation::new(0.02).unwrap(),
            FallSpeedMoments::from_mean_variance(zh, mean, variance).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn additive_quantities_map_to_existing_accumulator_shape() {
        let quantities = sample(100.0, 4.0, 2.25)
            .to_polar_accumulator_quantities()
            .unwrap();
        assert_eq!(quantities.zh, 100.0);
        assert_eq!(quantities.zv, 80.0);
        assert_eq!(quantities.cov_re, 50.0);
        assert_eq!(quantities.cov_im, -10.0);
        assert_eq!(quantities.kdp_deg_km, 0.7);
        assert!((quantities.fall_speed_mps - 4.0).abs() < 1.0e-6);
        assert!((quantities.fall_speed_variance_m2s2 - 2.25).abs() < 1.0e-6);
    }

    #[test]
    fn addition_combines_fall_speed_raw_moments_not_derived_variances() {
        let mixed = sample(100.0, 2.0, 1.0)
            .checked_add(sample(300.0, 6.0, 1.0))
            .unwrap()
            .to_polar_accumulator_quantities()
            .unwrap();
        assert!((mixed.fall_speed_mps - 5.0).abs() < 1.0e-6);
        // Within-population variance 1 plus between-population variance 3.
        assert!((mixed.fall_speed_variance_m2s2 - 4.0).abs() < 1.0e-6);
    }

    #[test]
    fn valid_fall_moments_survive_exact_reported_subnormal_node_scaling() {
        let scaled_zh = 2.848_208_6e-316;
        let mean = 1.950_608_138_469_241_3;
        let scaled = sample(1.0, mean, 0.0).checked_scale(scaled_zh).unwrap();
        let components = scaled.components();
        assert_eq!(components[0], scaled_zh);
        assert_eq!(components[7], 5.555_738_9e-316);
        assert_eq!(components[8], 1.083_706_947e-315);
        let (derived_mean, variance) = scaled.fall_speed.mean_variance(scaled.zh).unwrap();
        assert!((derived_mean - mean).abs() < 1.0e-15);
        assert_eq!(variance, 0.0);

        // Four subnormal quanta below the reported second moment is outside
        // even the exact half-ULP feasibility intervals and must still fail.
        let mut invalid = components;
        invalid[8] = f64::from_bits(invalid[8].to_bits() - 4);
        assert!(matches!(
            AdditiveScattering::from_components(invalid),
            Err(OutputError::NegativeFallSpeedVariance { .. })
        ));
    }

    #[test]
    fn scaling_collapses_fall_moments_when_zh_underflows_to_zero() {
        let minimum_subnormal = f64::from_bits(1);
        let scaled = sample(0.25, 8.0, 0.0)
            .checked_scale(minimum_subnormal)
            .unwrap();
        let components = scaled.components();
        assert_eq!(components[0], 0.0);
        assert_eq!(components[7], 0.0);
        assert_eq!(components[8], 0.0);

        // An invalid unscaled contribution remains fail-closed; the repair is
        // deliberately confined to underflow produced by checked scaling.
        assert!(matches!(
            AdditiveScattering::from_components([
                0.0,
                0.0,
                0.0,
                0.0,
                0.0,
                0.0,
                0.0,
                minimum_subnormal,
                minimum_subnormal,
            ]),
            Err(OutputError::FallMomentsWithoutReflectivity)
        ));
    }

    #[test]
    fn covariance_and_fall_moment_invariants_fail_closed() {
        let covariance_error =
            AdditiveScattering::from_components([4.0, 9.0, 6.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
                .unwrap_err();
        assert!(matches!(
            covariance_error,
            OutputError::CovarianceBound { .. }
        ));

        let fall_error =
            AdditiveScattering::from_components([10.0, 10.0, 0.0, 0.0, 0.0, 0.0, 0.0, 20.0, 39.0])
                .unwrap_err();
        assert!(matches!(
            fall_error,
            OutputError::NegativeFallSpeedVariance { .. }
        ));
    }

    #[test]
    fn signed_kdp_survives_the_app_seam_without_clamping() {
        let mut components = sample(10.0, 1.0, 0.0).components();
        components[4] = -0.25;
        let output = AdditiveScattering::from_components(components).unwrap();
        assert_eq!(
            output.to_polar_accumulator_quantities().unwrap().kdp_deg_km,
            -0.25
        );
    }

    #[test]
    fn signed_kdp_app_seam_uses_a_strict_symmetric_range() {
        for value in [-ACCUMULATOR_MAX_KDP_DEG_KM, ACCUMULATOR_MAX_KDP_DEG_KM] {
            let mut components = sample(10.0, 1.0, 0.0).components();
            components[4] = value;
            assert_eq!(
                AdditiveScattering::from_components(components)
                    .unwrap()
                    .to_polar_accumulator_quantities()
                    .unwrap()
                    .kdp_deg_km,
                value as f32
            );
        }

        for value in [
            -ACCUMULATOR_MAX_KDP_DEG_KM - 0.1,
            ACCUMULATOR_MAX_KDP_DEG_KM + 0.1,
        ] {
            let mut components = sample(10.0, 1.0, 0.0).components();
            components[4] = value;
            assert!(matches!(
                AdditiveScattering::from_components(components)
                    .unwrap()
                    .to_polar_accumulator_quantities(),
                Err(OutputError::PolarAccumulatorRange {
                    field: "KDP",
                    minimum: -1_000.0,
                    maximum: 1_000.0,
                    ..
                })
            ));
        }
    }

    #[test]
    fn app_seam_rejects_values_the_current_accumulator_would_saturate() {
        let output = AdditiveScattering::from_components([
            1.1e15, 1.1e15, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        ])
        .unwrap();
        assert!(matches!(
            output.to_polar_accumulator_quantities(),
            Err(OutputError::PolarAccumulatorRange { field: "ZH", .. })
        ));
    }
}
