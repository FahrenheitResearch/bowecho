use thiserror::Error;

const ORTHONORMAL_TOLERANCE: f64 = 1.0e-10;

/// Minimal finite complex number used for amplitude transforms.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Complex64 {
    re: f64,
    im: f64,
}

impl Complex64 {
    pub const ZERO: Self = Self { re: 0.0, im: 0.0 };

    pub fn new(re: f64, im: f64) -> Result<Self, OrientationError> {
        if !re.is_finite() || !im.is_finite() {
            return Err(OrientationError::NonFiniteComplex { re, im });
        }
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

    fn scaled(self, scale: f64) -> Self {
        Self {
            re: self.re * scale,
            im: self.im * scale,
        }
    }

    fn plus(self, other: Self) -> Self {
        Self {
            re: self.re + other.re,
            im: self.im + other.im,
        }
    }
}

/// An explicitly unit-length vector in the east/north/up basis.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UnitVector3([f64; 3]);

impl UnitVector3 {
    pub fn from_components(x: f64, y: f64, z: f64) -> Result<Self, OrientationError> {
        let components = [x, y, z];
        if components.iter().any(|value| !value.is_finite()) {
            return Err(OrientationError::NonFiniteVector { components });
        }
        let norm = x.hypot(y).hypot(z);
        if (norm - 1.0).abs() > ORTHONORMAL_TOLERANCE {
            return Err(OrientationError::NotUnitVector { norm });
        }
        Ok(Self(components))
    }

    /// Explicitly normalize a nonzero finite vector.
    pub fn normalize(x: f64, y: f64, z: f64) -> Result<Self, OrientationError> {
        let components = [x, y, z];
        if components.iter().any(|value| !value.is_finite()) {
            return Err(OrientationError::NonFiniteVector { components });
        }
        let norm = x.hypot(y).hypot(z);
        if norm == 0.0 || !norm.is_finite() {
            return Err(OrientationError::ZeroVector);
        }
        Self::from_components(x / norm, y / norm, z / norm)
    }

    #[must_use]
    pub const fn components(self) -> [f64; 3] {
        self.0
    }
}

/// Active rotation from particle body coordinates into east/north/up.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BodyOrientation {
    body_to_enu: [[f64; 3]; 3],
}

impl BodyOrientation {
    #[must_use]
    pub const fn identity() -> Self {
        Self {
            body_to_enu: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        }
    }

    pub fn from_matrix(body_to_enu: [[f64; 3]; 3]) -> Result<Self, OrientationError> {
        validate_rotation(body_to_enu)?;
        Ok(Self { body_to_enu })
    }

    /// Active right-handed axis-angle rotation, with the axis expressed in ENU.
    pub fn from_axis_angle(
        axis_enu: UnitVector3,
        angle_deg: f64,
    ) -> Result<Self, OrientationError> {
        if !angle_deg.is_finite() {
            return Err(OrientationError::NonFiniteAngle { angle_deg });
        }
        let [x, y, z] = axis_enu.components();
        let angle = angle_deg.to_radians();
        let c = angle.cos();
        let s = angle.sin();
        let one_minus_c = 1.0 - c;
        Self::from_matrix([
            [
                c + x * x * one_minus_c,
                x * y * one_minus_c - z * s,
                x * z * one_minus_c + y * s,
            ],
            [
                y * x * one_minus_c + z * s,
                c + y * y * one_minus_c,
                y * z * one_minus_c - x * s,
            ],
            [
                z * x * one_minus_c - y * s,
                z * y * one_minus_c + x * s,
                c + z * z * one_minus_c,
            ],
        ])
    }

    /// Active `Rz(yaw) * Ry(pitch) * Rx(roll)` rotation in degrees.
    pub fn from_euler_zyx_deg(
        yaw_deg: f64,
        pitch_deg: f64,
        roll_deg: f64,
    ) -> Result<Self, OrientationError> {
        if [yaw_deg, pitch_deg, roll_deg]
            .iter()
            .any(|angle| !angle.is_finite())
        {
            return Err(OrientationError::NonFiniteEuler {
                yaw_deg,
                pitch_deg,
                roll_deg,
            });
        }
        let (sy, cy) = yaw_deg.to_radians().sin_cos();
        let (sp, cp) = pitch_deg.to_radians().sin_cos();
        let (sr, cr) = roll_deg.to_radians().sin_cos();
        Self::from_matrix([
            [cy * cp, cy * sp * sr - sy * cr, cy * sp * cr + sy * sr],
            [sy * cp, sy * sp * sr + cy * cr, sy * sp * cr - cy * sr],
            [-sp, cp * sr, cp * cr],
        ])
    }

    #[must_use]
    pub const fn matrix(self) -> [[f64; 3]; 3] {
        self.body_to_enu
    }

    fn enu_to_body(self, vector: [f64; 3]) -> [f64; 3] {
        let mut result = [0.0; 3];
        for (body_axis, output) in result.iter_mut().enumerate() {
            *output = (0..3)
                .map(|enu_axis| self.body_to_enu[enu_axis][body_axis] * vector[enu_axis])
                .sum();
        }
        result
    }
}

/// Radar beam and H/V polarization basis in east/north/up coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RadarGeometry {
    azimuth_deg: f64,
    elevation_deg: f64,
    propagation_enu: UnitVector3,
    horizontal_enu: UnitVector3,
    vertical_enu: UnitVector3,
}

impl RadarGeometry {
    /// Build the conventional local ENU basis (azimuth clockwise from north).
    pub fn from_azimuth_elevation_deg(
        azimuth_deg: f64,
        elevation_deg: f64,
    ) -> Result<Self, OrientationError> {
        if !azimuth_deg.is_finite() || !(0.0..360.0).contains(&azimuth_deg) {
            return Err(OrientationError::AzimuthRange { azimuth_deg });
        }
        if !elevation_deg.is_finite() || !(-90.0..=90.0).contains(&elevation_deg) {
            return Err(OrientationError::ElevationRange { elevation_deg });
        }
        let (sin_az, cos_az) = azimuth_deg.to_radians().sin_cos();
        let (sin_el, cos_el) = elevation_deg.to_radians().sin_cos();
        let propagation_enu = UnitVector3::normalize(sin_az * cos_el, cos_az * cos_el, sin_el)?;
        let horizontal_enu = UnitVector3::normalize(cos_az, -sin_az, 0.0)?;
        // Upward-positive member of the transverse basis.
        let vertical_enu = UnitVector3::normalize(-sin_az * sin_el, -cos_az * sin_el, cos_el)?;
        Ok(Self {
            azimuth_deg,
            elevation_deg,
            propagation_enu,
            horizontal_enu,
            vertical_enu,
        })
    }

    #[must_use]
    pub const fn azimuth_deg(self) -> f64 {
        self.azimuth_deg
    }

    #[must_use]
    pub const fn elevation_deg(self) -> f64 {
        self.elevation_deg
    }

    #[must_use]
    pub const fn propagation_enu(self) -> UnitVector3 {
        self.propagation_enu
    }

    #[must_use]
    pub const fn horizontal_enu(self) -> UnitVector3 {
        self.horizontal_enu
    }

    #[must_use]
    pub const fn vertical_enu(self) -> UnitVector3 {
        self.vertical_enu
    }
}

/// Complex symmetric 3x3 scattering tensor in the particle body frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SymmetricScatteringTensor {
    xx: Complex64,
    yy: Complex64,
    zz: Complex64,
    xy: Complex64,
    xz: Complex64,
    yz: Complex64,
}

impl SymmetricScatteringTensor {
    #[must_use]
    pub const fn new(
        xx: Complex64,
        yy: Complex64,
        zz: Complex64,
        xy: Complex64,
        xz: Complex64,
        yz: Complex64,
    ) -> Self {
        Self {
            xx,
            yy,
            zz,
            xy,
            xz,
            yz,
        }
    }

    #[must_use]
    pub const fn from_diagonal(xx: Complex64, yy: Complex64, zz: Complex64) -> Self {
        Self::new(
            xx,
            yy,
            zz,
            Complex64::ZERO,
            Complex64::ZERO,
            Complex64::ZERO,
        )
    }

    fn matrix(self) -> [[Complex64; 3]; 3] {
        [
            [self.xx, self.xy, self.xz],
            [self.xy, self.yy, self.yz],
            [self.xz, self.yz, self.zz],
        ]
    }

    fn project(self, left: [f64; 3], right: [f64; 3]) -> Result<Complex64, OrientationError> {
        let matrix = self.matrix();
        let mut result = Complex64::ZERO;
        for row in 0..3 {
            for column in 0..3 {
                result = result.plus(matrix[row][column].scaled(left[row] * right[column]));
            }
        }
        Complex64::new(result.re, result.im)
    }
}

/// Body-frame forward and backscatter tensors.
///
/// A kernel must evaluate these tensors for the intended incident/scattered
/// propagation directions before this basis transform. This type does not
/// assume a monostatic reversal convention or solve a new scattering geometry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BodyFrameScattering {
    pub backscatter: SymmetricScatteringTensor,
    pub forward_scatter: SymmetricScatteringTensor,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RadarJonesMatrix {
    pub hh: Complex64,
    pub hv: Complex64,
    pub vh: Complex64,
    pub vv: Complex64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RadarScattering {
    pub backscatter: RadarJonesMatrix,
    pub forward_scatter: RadarJonesMatrix,
}

/// Rotate body-frame amplitudes into the radar H/V basis.
///
/// This is an amplitude-basis transform only. Calibration constants, PSD
/// integration, orientation-distribution averaging, KDP sign convention, and
/// attenuation conversion remain the responsibility of the selected kernel.
pub fn transform_body_to_radar(
    body: BodyFrameScattering,
    orientation: BodyOrientation,
    radar: RadarGeometry,
) -> Result<RadarScattering, OrientationError> {
    let horizontal_body = orientation.enu_to_body(radar.horizontal_enu.components());
    let vertical_body = orientation.enu_to_body(radar.vertical_enu.components());
    Ok(RadarScattering {
        backscatter: project_jones(body.backscatter, horizontal_body, vertical_body)?,
        forward_scatter: project_jones(body.forward_scatter, horizontal_body, vertical_body)?,
    })
}

fn project_jones(
    tensor: SymmetricScatteringTensor,
    horizontal_body: [f64; 3],
    vertical_body: [f64; 3],
) -> Result<RadarJonesMatrix, OrientationError> {
    let hh = tensor.project(horizontal_body, horizontal_body)?;
    let hv = tensor.project(horizontal_body, vertical_body)?;
    let vh = tensor.project(vertical_body, horizontal_body)?;
    let vv = tensor.project(vertical_body, vertical_body)?;
    Ok(RadarJonesMatrix { hh, hv, vh, vv })
}

fn validate_rotation(matrix: [[f64; 3]; 3]) -> Result<(), OrientationError> {
    if matrix.iter().flatten().any(|value| !value.is_finite()) {
        return Err(OrientationError::NonFiniteRotation);
    }
    for column in 0..3 {
        let norm: f64 = (0..3).map(|row| matrix[row][column].powi(2)).sum();
        if (norm - 1.0).abs() > ORTHONORMAL_TOLERANCE {
            return Err(OrientationError::NonOrthonormalRotation);
        }
        for other in (column + 1)..3 {
            let dot: f64 = (0..3)
                .map(|row| matrix[row][column] * matrix[row][other])
                .sum();
            if dot.abs() > ORTHONORMAL_TOLERANCE {
                return Err(OrientationError::NonOrthonormalRotation);
            }
        }
    }
    let determinant = matrix[0][0] * (matrix[1][1] * matrix[2][2] - matrix[1][2] * matrix[2][1])
        - matrix[0][1] * (matrix[1][0] * matrix[2][2] - matrix[1][2] * matrix[2][0])
        + matrix[0][2] * (matrix[1][0] * matrix[2][1] - matrix[1][1] * matrix[2][0]);
    if (determinant - 1.0).abs() > ORTHONORMAL_TOLERANCE {
        return Err(OrientationError::ImproperRotation { determinant });
    }
    Ok(())
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum OrientationError {
    #[error("complex amplitude must be finite, got ({re}, {im})")]
    NonFiniteComplex { re: f64, im: f64 },
    #[error("vector components must be finite, got {components:?}")]
    NonFiniteVector { components: [f64; 3] },
    #[error("vector must have unit norm, got {norm}")]
    NotUnitVector { norm: f64 },
    #[error("cannot normalize a zero vector")]
    ZeroVector,
    #[error("axis-angle rotation angle must be finite, got {angle_deg}")]
    NonFiniteAngle { angle_deg: f64 },
    #[error("Euler angles must be finite, got yaw={yaw_deg}, pitch={pitch_deg}, roll={roll_deg}")]
    NonFiniteEuler {
        yaw_deg: f64,
        pitch_deg: f64,
        roll_deg: f64,
    },
    #[error("rotation matrix contains a non-finite value")]
    NonFiniteRotation,
    #[error("rotation matrix must be orthonormal")]
    NonOrthonormalRotation,
    #[error("rotation matrix must be proper with determinant +1, got {determinant}")]
    ImproperRotation { determinant: f64 },
    #[error("azimuth must be finite in [0, 360), got {azimuth_deg}")]
    AzimuthRange { azimuth_deg: f64 },
    #[error("elevation must be finite in [-90, 90], got {elevation_deg}")]
    ElevationRange { elevation_deg: f64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn real(value: f64) -> Complex64 {
        Complex64::new(value, 0.0).unwrap()
    }

    fn body_tensor() -> BodyFrameScattering {
        let tensor = SymmetricScatteringTensor::from_diagonal(real(1.0), real(2.0), real(3.0));
        BodyFrameScattering {
            backscatter: tensor,
            forward_scatter: tensor,
        }
    }

    #[test]
    fn identity_orientation_projects_body_axes_into_radar_hv() {
        // Northward horizontal beam: H=east (body x), V=up (body z).
        let transformed = transform_body_to_radar(
            body_tensor(),
            BodyOrientation::identity(),
            RadarGeometry::from_azimuth_elevation_deg(0.0, 0.0).unwrap(),
        )
        .unwrap();
        assert_eq!(transformed.backscatter.hh, real(1.0));
        assert_eq!(transformed.backscatter.vv, real(3.0));
        assert_eq!(transformed.backscatter.hv, Complex64::ZERO);
        assert_eq!(transformed.backscatter.vh, Complex64::ZERO);
    }

    #[test]
    fn canting_about_beam_rotates_copolar_and_crosspolar_amplitudes() {
        let radar = RadarGeometry::from_azimuth_elevation_deg(0.0, 0.0).unwrap();
        let beam_axis = UnitVector3::from_components(0.0, 1.0, 0.0).unwrap();
        let forty_five = transform_body_to_radar(
            body_tensor(),
            BodyOrientation::from_axis_angle(beam_axis, 45.0).unwrap(),
            radar,
        )
        .unwrap()
        .backscatter;
        assert!((forty_five.hh.re() - 2.0).abs() < 1.0e-12);
        assert!((forty_five.vv.re() - 2.0).abs() < 1.0e-12);
        assert!((forty_five.hv.re().abs() - 1.0).abs() < 1.0e-12);
        assert_eq!(forty_five.hv, forty_five.vh);

        let ninety = transform_body_to_radar(
            body_tensor(),
            BodyOrientation::from_axis_angle(beam_axis, 90.0).unwrap(),
            radar,
        )
        .unwrap()
        .backscatter;
        assert!((ninety.hh.re() - 3.0).abs() < 1.0e-12);
        assert!((ninety.vv.re() - 1.0).abs() < 1.0e-12);
        assert!(ninety.hv.re().abs() < 1.0e-12);
    }

    #[test]
    fn invalid_rotation_and_radar_geometry_fail_closed() {
        assert!(matches!(
            BodyOrientation::from_matrix([[2.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]),
            Err(OrientationError::NonOrthonormalRotation)
        ));
        assert_eq!(
            RadarGeometry::from_azimuth_elevation_deg(360.0, 0.0),
            Err(OrientationError::AzimuthRange { azimuth_deg: 360.0 })
        );
    }

    #[test]
    fn isotropic_tensor_is_invariant_under_full_three_dimensional_rotation() {
        let isotropic = SymmetricScatteringTensor::from_diagonal(
            Complex64::new(2.0, -0.5).unwrap(),
            Complex64::new(2.0, -0.5).unwrap(),
            Complex64::new(2.0, -0.5).unwrap(),
        );
        let transformed = transform_body_to_radar(
            BodyFrameScattering {
                backscatter: isotropic,
                forward_scatter: isotropic,
            },
            BodyOrientation::from_euler_zyx_deg(37.0, -21.0, 68.0).unwrap(),
            RadarGeometry::from_azimuth_elevation_deg(123.0, 14.0).unwrap(),
        )
        .unwrap();
        let expected = Complex64::new(2.0, -0.5).unwrap();
        assert!((transformed.backscatter.hh.re() - expected.re()).abs() < 1.0e-12);
        assert!((transformed.backscatter.hh.im() - expected.im()).abs() < 1.0e-12);
        assert!((transformed.backscatter.vv.re() - expected.re()).abs() < 1.0e-12);
        assert!((transformed.backscatter.vv.im() - expected.im()).abs() < 1.0e-12);
        assert!(transformed.backscatter.hv.re().abs() < 1.0e-12);
        assert!(transformed.backscatter.hv.im().abs() < 1.0e-12);
    }
}
