//! Pure geometry for the satellite NATIVE-RESOLUTION window: a user-picked
//! lat/lon box (a typhoon eye, an island convection cluster) that composite
//! ingests fetch/decode at full instrument resolution instead of the
//! default 4× decimation of the whole sector.
//!
//! Everything here is side-effect-free math so it unit-tests offline:
//! window → scan-angle rect (per satellite navigation), scan-angle rect →
//! pixel/segment crop. The IO glue (downloads, HSD decode, store writes)
//! stays in `sat_worker`.
//!
//! Navigation conventions: GOES ABI is CF `sweep_angle_axis = "x"` and uses
//! rw-sat's `lat_lon_to_scan_angles_fast` directly. Himawari AHI is CF
//! sweep=y, which the pinned rw-sat navigates incorrectly (see
//! `sat_worker::ahi_scan_angles_to_lat_lon`), so the AHI forward navigation
//! lives here as the exact inverse of that function.

/// Mean km per degree of latitude (spherical earth, IUGG mean radius) —
/// only used to turn `size_km` into a lat/lon sampling box, never for
/// pixel-accurate navigation.
const KM_PER_DEG: f64 = 111.195;

/// A user-selected spatial window for native-resolution composite ingest:
/// a square of `size_km` per side centered on `center_lat_deg` /
/// `center_lon_deg`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SatNativeWindow {
    pub center_lat_deg: f64,
    pub center_lon_deg: f64,
    pub size_km: f64,
}

impl SatNativeWindow {
    /// Practical window-size bounds: below ~50 km a window is thinner than
    /// the resampling pad; above 2000 km the "window" is a sector and the
    /// native-res arrays stop being small (2000 km at 0.5 km = 4000×4000).
    pub const MIN_SIZE_KM: f64 = 50.0;
    pub const MAX_SIZE_KM: f64 = 2000.0;

    /// Clamp to the supported domain: size within bounds, latitude within
    /// the usable geostationary view (beyond ~75° the limb geometry breaks
    /// down long before the pole), longitude normalized to [-180, 180).
    pub fn clamped(self) -> Self {
        Self {
            center_lat_deg: self.center_lat_deg.clamp(-75.0, 75.0),
            center_lon_deg: (self.center_lon_deg + 180.0).rem_euclid(360.0) - 180.0,
            size_km: self.size_km.clamp(Self::MIN_SIZE_KM, Self::MAX_SIZE_KM),
        }
    }

    /// Half-extent of the window as a lat/lon box in degrees; the longitude
    /// half-span widens with latitude (cos clamped so polar inputs stay
    /// finite — `clamped()` keeps real windows out of that regime anyway).
    pub fn half_extent_deg(&self) -> (f64, f64) {
        let half_lat = 0.5 * self.size_km / KM_PER_DEG;
        let cos_lat = self.center_lat_deg.to_radians().cos().max(0.2);
        (half_lat, half_lat / cos_lat)
    }

    /// The center plus the corners and edge midpoints of the lat/lon box —
    /// the samples whose scan angles bound the crop. Eight boundary points
    /// are enough: scan-angle distortion across a ≤2000 km box is smooth and
    /// monotone away from the limb, and every crop adds pixel padding.
    pub fn sample_points(&self) -> [(f64, f64); 9] {
        let (dlat, dlon) = self.half_extent_deg();
        let (lat, lon) = (self.center_lat_deg, self.center_lon_deg);
        [
            (lat, lon),
            (lat - dlat, lon - dlon),
            (lat - dlat, lon),
            (lat - dlat, lon + dlon),
            (lat, lon - dlon),
            (lat, lon + dlon),
            (lat + dlat, lon - dlon),
            (lat + dlat, lon),
            (lat + dlat, lon + dlon),
        ]
    }

    /// Deterministic store token for run naming: lat/lon in tenths of a
    /// degree plus the size in km, e.g. 13.5N 144.8E 800 km →
    /// `win135n1448e800`. Same window → same token → frames of successive
    /// scans join one run dir and loop in the player.
    pub fn run_slug(&self) -> String {
        let clamped = self.clamped();
        let lat_tenths = (clamped.center_lat_deg * 10.0).round() as i64;
        let lon_tenths = (clamped.center_lon_deg * 10.0).round() as i64;
        format!(
            "win{}{}{}{}{}",
            lat_tenths.abs(),
            if lat_tenths < 0 { 's' } else { 'n' },
            lon_tenths.abs(),
            if lon_tenths < 0 { 'w' } else { 'e' },
            clamped.size_km.round() as i64
        )
    }
}

/// Scan-angle bounding rect of a window in the instrument's fixed grid
/// (radians; x east-west, y north-south, both as stored in the source
/// files' scan-angle axes).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScanAngleRect {
    pub x_min: f64,
    pub x_max: f64,
    pub y_min: f64,
    pub y_max: f64,
}

/// Project every sampled window point through `forward` (geodetic lat/lon →
/// scan angles) and take the bounding rect. `None` when ANY sample fails to
/// project: a window that hangs past the limb cannot be cropped honestly,
/// so it is rejected outright rather than silently truncated.
pub fn window_scan_angle_rect(
    window: SatNativeWindow,
    mut forward: impl FnMut(f64, f64) -> Option<(f64, f64)>,
) -> Option<ScanAngleRect> {
    let mut rect: Option<ScanAngleRect> = None;
    for (lat, lon) in window.clamped().sample_points() {
        let (x, y) = forward(lat, lon)?;
        if !(x.is_finite() && y.is_finite()) {
            return None;
        }
        rect = Some(match rect {
            None => ScanAngleRect {
                x_min: x,
                x_max: x,
                y_min: y,
                y_max: y,
            },
            Some(rect) => ScanAngleRect {
                x_min: rect.x_min.min(x),
                x_max: rect.x_max.max(x),
                y_min: rect.y_min.min(y),
                y_max: rect.y_max.max(y),
            },
        });
    }
    rect
}

/// Index range `(start, count)` of the axis samples whose values fall in
/// `[lo, hi]`, widened by `pad` samples each side (clamped to the axis).
/// Handles ascending and descending axes (GOES/AHI y scan axes descend);
/// `None` when the interval misses the axis entirely.
pub fn axis_crop_range(axis: &[f64], lo: f64, hi: f64, pad: usize) -> Option<(usize, usize)> {
    if axis.is_empty() || !(lo.is_finite() && hi.is_finite()) || lo > hi {
        return None;
    }
    let mut first: Option<usize> = None;
    let mut last: Option<usize> = None;
    for (idx, &value) in axis.iter().enumerate() {
        if value >= lo && value <= hi {
            first.get_or_insert(idx);
            last = Some(idx);
        }
    }
    let (first, last) = (first?, last?);
    let start = first.saturating_sub(pad);
    let end = (last + pad).min(axis.len() - 1);
    Some((start, end - start + 1))
}

/// CF sweep=y forward navigation: geodetic lat/lon → AHI scan angles
/// (radians). The exact inverse of `sat_worker::ahi_scan_angles_to_lat_lon`
/// (PROJ `geos` sweep=y forward; same ellipsoid math as the GOES-R PUG
/// §5.1.2.8.1 with the gimbal decomposition in sweep=y order):
/// `x = atan(-sy/sx)`, `y = atan(sz / hypot(sx, sy))`. `None` when the
/// point is on the far side of the globe.
pub fn ahi_lat_lon_to_scan_angles(
    perspective_point_height_m: f64,
    semi_major_axis_m: f64,
    semi_minor_axis_m: f64,
    lon0_deg: f64,
    lat_deg: f64,
    lon_deg: f64,
) -> Option<(f64, f64)> {
    let h = perspective_point_height_m + semi_major_axis_m;
    let a = semi_major_axis_m;
    let b = semi_minor_axis_m;
    if !(h.is_finite() && lon0_deg.is_finite() && lat_deg.is_finite() && lon_deg.is_finite())
        || h <= 0.0
        || a <= 0.0
        || b <= 0.0
    {
        return None;
    }

    let lat = lat_deg.to_radians();
    let lon_delta = (lon_deg - lon0_deg).to_radians();
    let pol_by_eq = (b * b) / (a * a);
    let geocentric_lat = (pol_by_eq * lat.tan()).atan();
    let radius = b / (1.0 - (1.0 - pol_by_eq) * geocentric_lat.cos().powi(2)).sqrt();

    // Satellite-relative components (x toward the earth center, y east,
    // z north) — the same frame the inverse decomposes.
    let sx = h - radius * geocentric_lat.cos() * lon_delta.cos();
    let sy = -radius * geocentric_lat.cos() * lon_delta.sin();
    let sz = radius * geocentric_lat.sin();

    // GOES-R PUG visibility condition: the point must face the satellite.
    if h * (h - sx) < sy * sy + sz * sz / pol_by_eq {
        return None;
    }

    let x = (-sy / sx).atan();
    let y = (sz / sx.hypot(sy)).atan();
    (x.is_finite() && y.is_finite()).then_some((x, y))
}

/// Nominal Himawari-8/9 geometry (JMA HSD User's Guide: satellite distance
/// 42164 km above a 6378.137/6356.7523 km GRS80 ellipsoid, sub-lon 140.7E).
/// Used ONLY to pick which full-disk segments to download BEFORE any file
/// exists locally; every pixel crop afterwards uses the downloaded header's
/// own projection block #3.
pub const AHI_NOMINAL_HEIGHT_M: f64 = 35_785_863.0;
pub const AHI_NOMINAL_SEMI_MAJOR_M: f64 = 6_378_137.0;
pub const AHI_NOMINAL_SEMI_MINOR_M: f64 = 6_356_752.3;
pub const AHI_NOMINAL_SUB_LON_DEG: f64 = 140.7;

/// Nominal GOES-R series sub-satellite longitudes (the operational East /
/// West slots). Like [`AHI_NOMINAL_SUB_LON_DEG`], used ONLY for coarse
/// pre-fetch decisions — on-disk math always uses the fetched file's own
/// projection.
pub const GOES_EAST_SUB_LON_DEG: f64 = -75.2;
pub const GOES_WEST_SUB_LON_DEG: f64 = -137.0;

/// Nominal sub-longitude of the GOES satellite a spec slug names: the West
/// slot for goes17/goes18, the East slot otherwise (goes16/goes19 and the
/// default).
pub fn goes_nominal_sub_lon_deg(satellite_slug: &str) -> f64 {
    match satellite_slug.trim().to_ascii_lowercase().as_str() {
        "goes17" | "goes18" | "g17" | "g18" => GOES_WEST_SUB_LON_DEG,
        _ => GOES_EAST_SUB_LON_DEG,
    }
}

/// Coarse per-satellite visibility gate for the persisted native window:
/// the great-circle arc from the sub-satellite point (lat 0, `sub_lon_deg`)
/// to the window CENTER must stay under this. 78° is comfortably inside
/// the ~81.3° geostationary limb, leaving margin for the window's own
/// extent while never rejecting anything usable — imagery beyond ~78°
/// off-nadir is limb foreshortening anyway. The exact on-disk projection
/// check still runs in the ingest; this only keeps one persisted window
/// from being attached to a satellite that cannot possibly see it (a Guam
/// window failing every GOES composite load, a CONUS window failing every
/// Himawari one).
pub const WINDOW_VISIBLE_MAX_ARC_DEG: f64 = 78.0;

/// Whether a window's center is plausibly within view of a geostationary
/// satellite at `sub_lon_deg` (see [`WINDOW_VISIBLE_MAX_ARC_DEG`]).
pub fn window_visible_from_sub_lon(sub_lon_deg: f64, window: &SatNativeWindow) -> bool {
    let clamped = window.clamped();
    let lat = clamped.center_lat_deg.to_radians();
    let delta_lon = (clamped.center_lon_deg - sub_lon_deg).to_radians();
    // Spherical great-circle arc between (0, sub_lon) and the center.
    let cos_arc = lat.cos() * delta_lon.cos();
    cos_arc >= WINDOW_VISIBLE_MAX_ARC_DEG.to_radians().cos()
}

/// Nominal AHI full-disk 1 km line geometry (JMA HSD User's Guide block #3:
/// LOFF 5500.5, LFAC 40932549 over 11000 lines split into 10 segments).
/// Segment boundaries are identical fractions of the disk at every band
/// resolution, so the 1 km numbers select segments for 0.5/1/2 km alike.
const AHI_FLDK_1KM_LINES: f64 = 11_000.0;
const AHI_FLDK_1KM_LOFF: f64 = 5_500.5;
const AHI_FLDK_1KM_LFAC: f64 = 40_932_549.0;
const AHI_FLDK_SEGMENTS: i64 = 10;
/// Selection safety margin in 1 km lines: covers the tiny drift between the
/// nominal constants above and a real header (verified ≪ 1 line) plus the
/// crop's own pixel padding.
const AHI_SEGMENT_PAD_LINES: f64 = 64.0;

/// The contiguous 1-based full-disk segment range `(start, count)` whose
/// lines cover a scan-angle rect (computed with the nominal geometry — see
/// [`AHI_NOMINAL_HEIGHT_M`]). Clamps to S01..S10, so a rect touching the
/// limb degrades to fetching the edge segment rather than failing.
pub fn ahi_fldk_segment_range(rect: &ScanAngleRect) -> (u8, u8) {
    let line_of =
        |y_rad: f64| AHI_FLDK_1KM_LOFF - y_rad.to_degrees() * AHI_FLDK_1KM_LFAC / 65_536.0;
    // Lines grow southward while y scan angles shrink: y_max is the north
    // edge (smallest line number).
    let north_line = line_of(rect.y_max) - AHI_SEGMENT_PAD_LINES;
    let south_line = line_of(rect.y_min) + AHI_SEGMENT_PAD_LINES;
    let lines_per_segment = AHI_FLDK_1KM_LINES / AHI_FLDK_SEGMENTS as f64;
    let segment_of = |line: f64| {
        (((line - 1.0) / lines_per_segment).floor() as i64 + 1).clamp(1, AHI_FLDK_SEGMENTS)
    };
    let first = segment_of(north_line);
    let last = segment_of(south_line).max(first);
    (first as u8, (last - first + 1) as u8)
}

/// The pixel crop of one AHI band's fetched-segment block that covers a
/// scan-angle rect, with the cropped grid's own scan-angle axes. Column and
/// line scaling is the CGMS LRIT/HRIT normalized geostationary mapping —
/// `scan_deg = (coord - offset) · 65536 / factor` — byte-matching rw-sat's
/// (private) `himawari_column_scan_rad` / `himawari_line_scan_rad`, so a
/// windowed grid is exactly the corresponding slice of the full assembled
/// grid and successive scans reuse one run dir.
#[derive(Debug, Clone, PartialEq)]
pub struct AhiWindowCrop {
    /// Zero-based first column within the full-disk row, and column count.
    pub col_start: usize,
    pub col_count: usize,
    /// One-based absolute first line (HSD numbering), and line count.
    pub line_start: u32,
    pub line_count: usize,
    /// Scan-angle axes of the cropped grid (x ascending, y descending —
    /// the stored row order, north first).
    pub x_scan_rad: Vec<f64>,
    pub y_scan_rad: Vec<f64>,
}

/// Compute the crop for a band whose fetched segments span absolute lines
/// `first_line .. first_line + total_lines - 1` of a `columns`-wide grid,
/// using the real header scaling `(cfac, coff, lfac, loff)`. `pad` widens
/// the crop by that many pixels each side (clamped), keeping bilinear
/// cross-band resampling clear of the window edge.
#[allow(clippy::too_many_arguments)]
pub fn ahi_window_crop(
    cfac: f64,
    coff: f64,
    lfac: f64,
    loff: f64,
    columns: usize,
    first_line: u32,
    total_lines: usize,
    rect: &ScanAngleRect,
    pad: usize,
) -> Result<AhiWindowCrop, String> {
    if columns == 0 || total_lines == 0 || cfac <= 0.0 || lfac <= 0.0 {
        return Err("degenerate AHI grid for window crop".to_string());
    }
    // Fractional zero-based column / one-based line of a scan angle —
    // the inverses of the CGMS mapping above.
    let col_of = |x_rad: f64| coff + x_rad.to_degrees() * cfac / 65_536.0 - 1.0;
    let line_of = |y_rad: f64| loff - y_rad.to_degrees() * lfac / 65_536.0;

    let col_lo = col_of(rect.x_min).floor() as i64 - pad as i64;
    let col_hi = col_of(rect.x_max).ceil() as i64 + pad as i64;
    let col_start = col_lo.clamp(0, columns as i64 - 1) as usize;
    let col_end = col_hi.clamp(0, columns as i64 - 1) as usize;
    if col_end < col_start || col_hi < 0 || col_lo > columns as i64 - 1 {
        return Err("the window does not intersect the AHI columns".to_string());
    }

    let line_first = i64::from(first_line);
    let line_last = line_first + total_lines as i64 - 1;
    // y_max is the north edge = the SMALLEST line number.
    let line_lo = line_of(rect.y_max).floor() as i64 - pad as i64;
    let line_hi = line_of(rect.y_min).ceil() as i64 + pad as i64;
    if line_hi < line_first || line_lo > line_last {
        return Err("the window does not intersect the fetched AHI segments".to_string());
    }
    let line_start = line_lo.clamp(line_first, line_last);
    let line_end = line_hi.clamp(line_first, line_last);

    let x_scan_rad = (col_start..=col_end)
        .map(|col| ((col as f64 + 1.0 - coff) * 65_536.0 / cfac).to_radians())
        .collect::<Vec<_>>();
    let y_scan_rad = (line_start..=line_end)
        .map(|line| ((loff - line as f64) * 65_536.0 / lfac).to_radians())
        .collect::<Vec<_>>();

    Ok(AhiWindowCrop {
        col_start,
        col_count: col_end - col_start + 1,
        line_start: line_start as u32,
        line_count: (line_end - line_start + 1) as usize,
        x_scan_rad,
        y_scan_rad,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rw_sat::geostationary::{
        SweepAngleAxis, lat_lon_to_scan_angles_fast, scan_angles_to_lat_lon,
    };
    use rw_sat::store::SatelliteProjection;

    fn ahi_projection() -> SatelliteProjection {
        SatelliteProjection {
            perspective_point_height_m: AHI_NOMINAL_HEIGHT_M,
            semi_major_axis_m: AHI_NOMINAL_SEMI_MAJOR_M,
            semi_minor_axis_m: AHI_NOMINAL_SEMI_MINOR_M,
            longitude_of_projection_origin_deg: AHI_NOMINAL_SUB_LON_DEG,
            sweep_angle_axis: SweepAngleAxis::X,
        }
    }

    fn ahi_forward(lat: f64, lon: f64) -> Option<(f64, f64)> {
        ahi_lat_lon_to_scan_angles(
            AHI_NOMINAL_HEIGHT_M,
            AHI_NOMINAL_SEMI_MAJOR_M,
            AHI_NOMINAL_SEMI_MINOR_M,
            AHI_NOMINAL_SUB_LON_DEG,
            lat,
            lon,
        )
    }

    fn guam_window() -> SatNativeWindow {
        SatNativeWindow {
            center_lat_deg: 13.5,
            center_lon_deg: 144.8,
            size_km: 800.0,
        }
    }

    #[test]
    fn window_slug_is_deterministic_and_store_safe() {
        assert_eq!(guam_window().run_slug(), "win135n1448e800");
        let southern = SatNativeWindow {
            center_lat_deg: -8.25,
            center_lon_deg: -100.24,
            size_km: 500.0,
        };
        assert_eq!(southern.run_slug(), "win83s1002w500");
        // Store-token safe: lowercase alphanumerics only.
        for slug in [guam_window().run_slug(), southern.run_slug()] {
            assert!(
                slug.chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit()),
                "{slug}"
            );
        }
        // Out-of-domain inputs clamp instead of producing junk tokens.
        let wild = SatNativeWindow {
            center_lat_deg: 89.0,
            center_lon_deg: 361.0,
            size_km: 9_999.0,
        };
        assert_eq!(wild.run_slug(), "win750n10e2000");
    }

    #[test]
    fn axis_crop_range_handles_direction_pad_and_misses() {
        let ascending = [0.0, 1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(axis_crop_range(&ascending, 1.5, 3.5, 0), Some((2, 2)));
        assert_eq!(axis_crop_range(&ascending, 1.5, 3.5, 1), Some((1, 4)));
        // Pad clamps at both ends.
        assert_eq!(axis_crop_range(&ascending, 0.0, 5.0, 3), Some((0, 6)));
        let descending = [5.0, 4.0, 3.0, 2.0, 1.0, 0.0];
        assert_eq!(axis_crop_range(&descending, 1.5, 3.5, 0), Some((2, 2)));
        // No overlap → None.
        assert_eq!(axis_crop_range(&ascending, 7.0, 9.0, 2), None);
        assert_eq!(axis_crop_range(&[], 0.0, 1.0, 0), None);
        assert_eq!(axis_crop_range(&ascending, 3.0, 1.0, 0), None);
    }

    /// The AHI forward navigation is the exact inverse of the app's CF
    /// sweep=y inverse (`sat_worker::ahi_scan_angles_to_lat_lon`), pinned to
    /// the same pyproj 3.7.2 reference the sweep=y writer test uses:
    /// scan angles (0.04, 0.12) ↔ 46.296691N 160.862374E.
    #[test]
    fn ahi_forward_navigation_round_trips_the_app_inverse() {
        let (x, y) = ahi_forward(46.296691, 160.862374).expect("visible point");
        assert!((x - 0.04).abs() < 1.0e-6, "x={x}");
        assert!((y - 0.12).abs() < 1.0e-6, "y={y}");

        let projection = ahi_projection();
        for (lat, lon) in [
            (13.5, 144.8),
            (0.0, AHI_NOMINAL_SUB_LON_DEG),
            (-35.0, 120.0),
            (30.0, 179.5),
            (20.0, -170.0), // across the antimeridian, still on-disk
        ] {
            let (x, y) = ahi_forward(lat, lon).expect("visible point");
            let (back_lat, back_lon) =
                crate::sat_worker::ahi_scan_angles_to_lat_lon(&projection, x, y)
                    .expect("round trip");
            assert!((f64::from(back_lat) - lat).abs() < 1.0e-3, "{back_lat}");
            let lon_err = (f64::from(back_lon) - lon + 180.0).rem_euclid(360.0) - 180.0;
            assert!(lon_err.abs() < 1.0e-3, "{back_lon}");
        }
        // Sub-satellite point maps to the scan origin.
        let (x0, y0) = ahi_forward(0.0, AHI_NOMINAL_SUB_LON_DEG).unwrap();
        assert!(x0.abs() < 1.0e-9 && y0.abs() < 1.0e-9);
        // Far side of the globe is rejected.
        assert!(ahi_forward(0.0, -39.3).is_none());
    }

    /// The visibility gate keeps a persisted window from being attached to
    /// the satellite that cannot see it: Guam belongs to Himawari (and would
    /// fail every GOES-East load), Miami belongs to GOES (and would fail
    /// every Himawari load).
    #[test]
    fn native_window_visibility_is_gated_per_satellite() {
        let guam = guam_window();
        assert!(window_visible_from_sub_lon(AHI_NOMINAL_SUB_LON_DEG, &guam));
        assert!(!window_visible_from_sub_lon(GOES_EAST_SUB_LON_DEG, &guam));

        let miami = SatNativeWindow {
            center_lat_deg: 25.8,
            center_lon_deg: -80.2,
            size_km: 500.0,
        };
        assert!(window_visible_from_sub_lon(GOES_EAST_SUB_LON_DEG, &miami));
        assert!(window_visible_from_sub_lon(GOES_WEST_SUB_LON_DEG, &miami));
        assert!(!window_visible_from_sub_lon(
            AHI_NOMINAL_SUB_LON_DEG,
            &miami
        ));

        // The gate agrees with the exact forward navigation it pre-screens
        // for: what it accepts projects, what it rejects does not.
        assert!(window_scan_angle_rect(guam, ahi_forward).is_some());
        assert!(window_scan_angle_rect(miami, ahi_forward).is_none());

        // Spec slugs map onto the operational GOES slots; unknown slugs
        // default to East (today's `goes19` default source).
        assert_eq!(goes_nominal_sub_lon_deg("goes19"), GOES_EAST_SUB_LON_DEG);
        assert_eq!(goes_nominal_sub_lon_deg("goes16"), GOES_EAST_SUB_LON_DEG);
        assert_eq!(goes_nominal_sub_lon_deg("goes18"), GOES_WEST_SUB_LON_DEG);
        assert_eq!(goes_nominal_sub_lon_deg(" GOES17 "), GOES_WEST_SUB_LON_DEG);
    }

    #[test]
    fn window_rect_bounds_the_center_and_rejects_off_disk_windows() {
        let rect = window_scan_angle_rect(guam_window(), ahi_forward).expect("rect");
        let (cx, cy) = ahi_forward(13.5, 144.8).unwrap();
        assert!(rect.x_min < cx && cx < rect.x_max);
        assert!(rect.y_min < cy && cy < rect.y_max);
        // ~800 km spans ~0.022 rad from geostationary orbit; allow slack for
        // the off-nadir stretch.
        for span in [rect.x_max - rect.x_min, rect.y_max - rect.y_min] {
            assert!((0.018..=0.045).contains(&span), "span={span}");
        }

        // A window centered on the far side of the globe projects nowhere.
        let far = SatNativeWindow {
            center_lat_deg: 20.0,
            center_lon_deg: -39.3,
            size_km: 800.0,
        };
        assert!(window_scan_angle_rect(far, ahi_forward).is_none());

        // GOES sweep=x forward (rw-sat) drives the same rect machinery.
        let goes_forward = |lat: f64, lon: f64| {
            lat_lon_to_scan_angles_fast(
                35_786_023.0,
                6_378_137.0,
                6_356_752.314_14,
                -75.0,
                SweepAngleAxis::X,
                lat,
                lon,
            )
        };
        let houston = SatNativeWindow {
            center_lat_deg: 29.7,
            center_lon_deg: -95.4,
            size_km: 800.0,
        };
        let rect = window_scan_angle_rect(houston, goes_forward).expect("goes rect");
        let (cx, cy) = goes_forward(29.7, -95.4).unwrap();
        assert!(rect.x_min < cx && cx < rect.x_max);
        assert!(rect.y_min < cy && cy < rect.y_max);
        for span in [rect.x_max - rect.x_min, rect.y_max - rect.y_min] {
            assert!((0.018..=0.045).contains(&span), "span={span}");
        }
    }

    /// Guam (13.5N) sits in segment 4 of the 10-segment full disk; an
    /// 800 km window around it needs S04-S05 — exactly the tropical band the
    /// non-windowed composite defaults to.
    #[test]
    fn segment_range_covers_the_guam_window() {
        let rect = window_scan_angle_rect(guam_window(), ahi_forward).expect("rect");
        assert_eq!(ahi_fldk_segment_range(&rect), (4, 2));

        // A big equatorial window spreads across more segments.
        let wide = SatNativeWindow {
            center_lat_deg: 0.0,
            center_lon_deg: 140.7,
            size_km: 2000.0,
        };
        let rect = window_scan_angle_rect(wide, ahi_forward).expect("rect");
        let (start, count) = ahi_fldk_segment_range(&rect);
        assert!(start <= 5 && start + count > 6, "{start}+{count}");

        // Near the northern limb the range clamps to S01.
        let north = SatNativeWindow {
            center_lat_deg: 70.0,
            center_lon_deg: 140.7,
            size_km: 400.0,
        };
        let rect = window_scan_angle_rect(north, ahi_forward).expect("rect");
        let (start, _) = ahi_fldk_segment_range(&rect);
        assert_eq!(start, 1);
    }

    /// End-to-end georef of a cropped grid: crop a Guam window out of the
    /// nominal 1 km S04-S05 block, then navigate the cropped axes back to
    /// lat/lon — the corners must bracket the requested box, the center must
    /// land on the window center within a pixel.
    #[test]
    fn ahi_window_crop_round_trips_georef() {
        let window = guam_window();
        let rect = window_scan_angle_rect(window, ahi_forward).expect("rect");
        // Nominal 1 km FLDK header values; segments 4-5 span lines 3301..5500.
        let (cfac, coff, lfac, loff) = (40_932_549.0, 5_500.5, 40_932_549.0, 5_500.5);
        let crop = ahi_window_crop(cfac, coff, lfac, loff, 11_000, 3_301, 2_200, &rect, 2)
            .expect("crop fits the fetched segments");

        // ~800 km of 1 km pixels plus limb stretch and padding.
        assert!(
            (750..=1_100).contains(&crop.col_count),
            "cols={}",
            crop.col_count
        );
        assert!(
            (750..=1_100).contains(&crop.line_count),
            "lines={}",
            crop.line_count
        );
        assert_eq!(crop.x_scan_rad.len(), crop.col_count);
        assert_eq!(crop.y_scan_rad.len(), crop.line_count);
        // Rows are stored north-first: y axis descends.
        assert!(crop.y_scan_rad[0] > *crop.y_scan_rad.last().unwrap());

        let projection = ahi_projection();
        let center = crate::sat_worker::ahi_scan_angles_to_lat_lon(
            &projection,
            crop.x_scan_rad[crop.col_count / 2],
            crop.y_scan_rad[crop.line_count / 2],
        )
        .expect("center navigates");
        assert!((f64::from(center.0) - 13.5).abs() < 0.05, "{center:?}");
        assert!((f64::from(center.1) - 144.8).abs() < 0.05, "{center:?}");

        // The cropped grid covers the whole requested box.
        let (dlat, dlon) = window.half_extent_deg();
        let nw = crate::sat_worker::ahi_scan_angles_to_lat_lon(
            &projection,
            crop.x_scan_rad[0],
            crop.y_scan_rad[0],
        )
        .expect("nw corner navigates");
        let se = crate::sat_worker::ahi_scan_angles_to_lat_lon(
            &projection,
            *crop.x_scan_rad.last().unwrap(),
            *crop.y_scan_rad.last().unwrap(),
        )
        .expect("se corner navigates");
        assert!(f64::from(nw.0) >= 13.5 + dlat - 0.02, "north {nw:?}");
        assert!(f64::from(se.0) <= 13.5 - dlat + 0.02, "south {se:?}");
        assert!(f64::from(nw.1) <= 144.8 - dlon + 0.02, "west {nw:?}");
        assert!(f64::from(se.1) >= 144.8 + dlon - 0.02, "east {se:?}");

        // A rect entirely outside the fetched block is refused.
        let off = ScanAngleRect {
            x_min: rect.x_min,
            x_max: rect.x_max,
            y_min: 0.14,
            y_max: 0.15,
        };
        assert!(ahi_window_crop(cfac, coff, lfac, loff, 11_000, 3_301, 2_200, &off, 2).is_err());
    }

    /// The nominal-geometry segment selection and the real-header crop stay
    /// in agreement: the crop's absolute lines fall inside the segments the
    /// selector picked (the 64-line pad absorbs nominal-vs-header drift).
    #[test]
    fn segment_selection_contains_the_crop() {
        for (lat, lon, km) in [
            (13.5, 144.8, 800.0),
            (-18.0, 155.0, 1_500.0),
            (35.0, 130.0, 300.0),
            (0.0, 140.7, 50.0),
        ] {
            let window = SatNativeWindow {
                center_lat_deg: lat,
                center_lon_deg: lon,
                size_km: km,
            };
            let rect = window_scan_angle_rect(window, ahi_forward).expect("rect");
            let (seg_start, seg_count) = ahi_fldk_segment_range(&rect);
            let first_line = (u32::from(seg_start) - 1) * 1_100 + 1;
            let total_lines = usize::from(seg_count) * 1_100;
            let crop = ahi_window_crop(
                40_932_549.0,
                5_500.5,
                40_932_549.0,
                5_500.5,
                11_000,
                first_line,
                total_lines,
                &rect,
                2,
            )
            .expect("crop fits the selected segments");
            // Unclamped fit: the crop must not be squeezed against either
            // edge of the fetched block unless the block itself is at the
            // disk edge.
            if seg_start > 1 {
                assert!(crop.line_start > first_line, "{lat} {lon} {km}");
            }
            let crop_last = crop.line_start + crop.line_count as u32 - 1;
            let block_last = first_line + total_lines as u32 - 1;
            if seg_start + seg_count - 1 < 10 {
                assert!(crop_last < block_last, "{lat} {lon} {km}");
            }
        }
    }

    /// The GOES sweep=x rect against rw-sat's own inverse: crop axes built
    /// from a synthetic CONUS-like axis pair round-trip to the window box.
    #[test]
    fn goes_axis_crop_round_trips_georef() {
        let (h, a, b, lon0) = (35_786_023.0, 6_378_137.0, 6_356_752.314_14, -75.0);
        let forward = |lat: f64, lon: f64| {
            lat_lon_to_scan_angles_fast(h, a, b, lon0, SweepAngleAxis::X, lat, lon)
        };
        let window = SatNativeWindow {
            center_lat_deg: 29.7,
            center_lon_deg: -95.4,
            size_km: 600.0,
        };
        let rect = window_scan_angle_rect(window, forward).expect("rect");

        // Synthetic 2 km CONUS-ish axes (x ascending, y descending).
        let x_axis: Vec<f64> = (0..2500).map(|i| -0.101332 + i as f64 * 5.6e-5).collect();
        let y_axis: Vec<f64> = (0..1500).map(|j| 0.128212 - j as f64 * 5.6e-5).collect();
        let (x_start, x_count) = axis_crop_range(&x_axis, rect.x_min, rect.x_max, 2).unwrap();
        let (y_start, y_count) = axis_crop_range(&y_axis, rect.y_min, rect.y_max, 2).unwrap();
        // ~600 km of 2 km pixels. Ground distance FORESHORTENS in scan
        // angle away from nadir on both axes (oblique view — Houston sits
        // ~20° off the sub-satellite point → ~250 samples, not 300).
        assert!((220..=420).contains(&x_count), "{x_count}");
        assert!((220..=420).contains(&y_count), "{y_count}");

        let center = scan_angles_to_lat_lon(
            h,
            a,
            b,
            lon0,
            SweepAngleAxis::X,
            x_axis[x_start + x_count / 2],
            y_axis[y_start + y_count / 2],
        )
        .expect("center navigates");
        assert!((f64::from(center.0) - 29.7).abs() < 0.1, "{center:?}");
        assert!((f64::from(center.1) + 95.4).abs() < 0.1, "{center:?}");
    }
}
