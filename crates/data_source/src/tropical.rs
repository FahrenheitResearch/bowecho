//! Tropical cyclone data — a unified model plus parsers for the two free,
//! keyless sources BowEcho aggregates:
//!
//! - **NHC** `CurrentStorms.json` — official for the Atlantic and East/Central
//!   Pacific; carries max wind (kt), min pressure (mb), and motion directly.
//! - **GDACS** `geteventlist/EVENTS4APP?eventtypes=TC` — a global aggregator
//!   (JTWC/JMA/etc.) that covers every other basin, including the West Pacific.
//!   Its `getgeometry` endpoint returns the track, cone, and impact polygons —
//!   but NO honest per-point forecast wind (it repeats the storm's *current*
//!   severity on every point).
//! - **JTWC** (Joint Typhoon Warning Center) Tropical Cyclone Warning text —
//!   the official U.S. forecast authority for the basins NHC does not cover
//!   (West Pacific, Indian Ocean, Southern Hemisphere). Its fixed-format
//!   `wpNNyyweb.txt` bulletin carries a per-point forecast track WITH max
//!   sustained wind (kt) at 12/24/36/48/72/96/120 h — the West-Pacific analogue
//!   of NHC's TCM. Active warnings are discovered from the JTWC RSS feed, then
//!   matched to a GDACS storm by name to enrich its forecast dots with real
//!   Saffir–Simpson intensity (the cone/track still come from GDACS). The
//!   warning's identity block ([`WarningInfo`]: warning number, issue DTG,
//!   analysis time/position, current wind/gusts/pressure/motion) is parsed too:
//!   it REPLACES the aggregator's lagging severity on the storm record (see
//!   [`sync_storm_with_geometry`]) and is shown on the card so the intensity's
//!   age is always visible.
//!
//! The two feed a single [`TropicalCyclone`] so the UI never cares which
//! center issued a storm. Fields are intentionally comprehensive (wind, gusts,
//! pressure, motion, category) — each source fills what it has, and richer
//! intensity sources (ATCF, CIMSS ADT) can enrich the same record later.
//!
//! Scales/definitions: Saffir–Simpson by 1-min max sustained wind (kt); the
//! West Pacific "(Super) Typhoon" labels map onto the same wind thresholds.

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use serde::Deserialize;

/// A lon/lat point in degrees (east/north positive), matching the app's other
/// geographic records.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeoPoint {
    pub lon: f32,
    pub lat: f32,
}

/// Ocean basin a storm lives in. Used for labeling ("Hurricane" vs "Typhoon")
/// and for grouping the storm list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Basin {
    Atlantic,
    EastPacific,
    CentralPacific,
    WestPacific,
    NorthIndian,
    SouthIndian,
    SouthPacific,
    Other,
}

impl Basin {
    /// Approximate basin from position — the fallback when a source does not
    /// name one (GDACS). Boundaries follow the WMO/agency areas of
    /// responsibility closely enough for labeling.
    pub fn from_lon_lat(lon: f32, lat: f32) -> Self {
        let lon = normalize_lon(lon);
        if lat >= 0.0 {
            if (-100.0..=0.0).contains(&lon) || lon > 0.0 && lon <= 20.0 {
                Basin::Atlantic
            } else if (-180.0..-140.0).contains(&lon) {
                Basin::CentralPacific
            } else if (-140.0..-100.0).contains(&lon) {
                Basin::EastPacific
            } else if (100.0..=180.0).contains(&lon) || (-180.0..-160.0).contains(&lon) {
                Basin::WestPacific
            } else if (30.0..100.0).contains(&lon) {
                Basin::NorthIndian
            } else {
                Basin::Other
            }
        } else if (30.0..135.0).contains(&lon) {
            Basin::SouthIndian
        } else if (135.0..=180.0).contains(&lon) || (-180.0..-70.0).contains(&lon) {
            Basin::SouthPacific
        } else {
            Basin::Other
        }
    }

    /// The intensity noun used in this basin at/above hurricane force.
    fn strong_noun(self) -> &'static str {
        match self {
            Basin::Atlantic | Basin::EastPacific | Basin::CentralPacific => "Hurricane",
            Basin::WestPacific => "Typhoon",
            _ => "Cyclone",
        }
    }

    /// JTWC supplies the official warning forecast outside the Atlantic and
    /// East/Central Pacific areas covered by NHC/CPHC. Name matching alone is
    /// not sufficient: storm names can be reused across basins and seasons.
    fn uses_jtwc_forecasts(self) -> bool {
        matches!(
            self,
            Basin::WestPacific
                | Basin::NorthIndian
                | Basin::SouthIndian
                | Basin::SouthPacific
                | Basin::Other
        )
    }
}

/// Which center/aggregator a record came from (shown in the card for honesty).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Nhc,
    Gdacs,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Nhc => "NHC",
            Source::Gdacs => "GDACS",
        }
    }
}

/// Which official center's bulletin a [`WarningInfo`] came from — drives the
/// product noun on the storm card ("JTWC Warning #25" vs "NHC Advisory #15").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarningAgency {
    Jtwc,
    Nhc,
}

impl WarningAgency {
    /// The product noun shown to the user, e.g. "JTWC Warning".
    pub fn product(self) -> &'static str {
        match self {
            WarningAgency::Jtwc => "JTWC Warning",
            WarningAgency::Nhc => "NHC Advisory",
        }
    }
}

/// The identity, timing, and analysis vitals of the official warning/advisory
/// bulletin behind a storm's forecast (a JTWC Tropical Cyclone Warning or an
/// NHC Forecast/Advisory). Two jobs:
///
/// 1. **Visible timing** — the card shows WHICH bulletin the numbers came from
///    and how old it is (`JTWC Warning #25 · issued 07/0300Z (4 h ago) ·
///    position 07/0000Z`), so stale data is never silent.
/// 2. **Official vitals** — for JTWC-covered storms the analysis wind/gusts/
///    pressure/position/motion replace the GDACS aggregate, which repeats a
///    storm's last-processed severity for many hours after JTWC has already
///    issued newer warnings (see [`sync_storm_with_geometry`]).
#[derive(Clone, Debug, PartialEq)]
pub struct WarningInfo {
    pub agency: WarningAgency,
    /// Warning/advisory sequence number (JTWC `WARNING NR 025`, NHC
    /// `FORECAST/ADVISORY NUMBER 15`).
    pub number: Option<u32>,
    /// When the bulletin was issued (JTWC WMO-header DTG / NHC datestamp).
    pub issued: Option<DateTime<Utc>>,
    /// The analysis time (`WARNING POSITION` / `CENTER LOCATED NEAR ... AT`).
    pub position_time: Option<DateTime<Utc>>,
    /// The analysis position.
    pub position: Option<GeoPoint>,
    /// Current max sustained wind (kt) at the analysis time.
    pub max_wind_kt: Option<f32>,
    pub gust_kt: Option<f32>,
    /// Recent motion (toward, degrees true / kt), where the bulletin states it.
    pub movement_dir_deg: Option<f32>,
    pub movement_speed_kt: Option<f32>,
    /// Minimum central pressure (mb), where the bulletin states it.
    pub min_pressure_mb: Option<f32>,
}

impl WarningInfo {
    /// `JTWC Warning #25` / `NHC Advisory #15` (number omitted when unknown).
    pub fn product_label(&self) -> String {
        match self.number {
            Some(number) => format!("{} #{number}", self.agency.product()),
            None => self.agency.product().to_owned(),
        }
    }

    /// The storm-card timing line: product identity, issue time (with its age
    /// relative to `now`), and analysis-position time — every part the
    /// bulletin carried, e.g.
    /// `JTWC Warning #25 · issued 07/0300Z (4 h ago) · position 07/0000Z`.
    pub fn identity_summary(&self, now: DateTime<Utc>) -> String {
        let mut parts = vec![self.product_label()];
        if let Some(issued) = self.issued {
            parts.push(format!(
                "issued {} ({})",
                issued.format("%d/%H%MZ"),
                age_label(now, issued)
            ));
        }
        if let Some(position_time) = self.position_time {
            parts.push(format!("position {}", position_time.format("%d/%H%MZ")));
        }
        parts.join(" · ")
    }
}

/// `5 min ago` / `4 h ago` / `2 d ago` (clamped at zero against clock skew).
pub fn age_label(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let mins = (now - then).num_minutes().max(0);
    if mins < 60 {
        format!("{mins} min ago")
    } else if mins < 60 * 24 {
        format!("{} h ago", mins / 60)
    } else {
        format!("{} d ago", mins / (60 * 24))
    }
}

/// Saffir–Simpson bin by 1-min max sustained wind (kt).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    TropicalDepression,
    TropicalStorm,
    One,
    Two,
    Three,
    Four,
    Five,
}

impl Category {
    /// Saffir–Simpson thresholds (kt): TD < 34, TS 34–63, 1: 64–82, 2: 83–95,
    /// 3: 96–112, 4: 113–136, 5: ≥ 137.
    pub fn from_wind_kt(kt: f32) -> Self {
        if kt < 34.0 {
            Category::TropicalDepression
        } else if kt < 64.0 {
            Category::TropicalStorm
        } else if kt < 83.0 {
            Category::One
        } else if kt < 96.0 {
            Category::Two
        } else if kt < 113.0 {
            Category::Three
        } else if kt < 137.0 {
            Category::Four
        } else {
            Category::Five
        }
    }

    /// A basin-aware label, e.g. "Category 4 Typhoon", "Super Typhoon",
    /// "Tropical Storm".
    pub fn label(self, basin: Basin) -> String {
        match self {
            Category::TropicalDepression => "Tropical Depression".to_owned(),
            Category::TropicalStorm => "Tropical Storm".to_owned(),
            Category::Five if basin == Basin::WestPacific => "Super Typhoon".to_owned(),
            Category::One => format!("Category 1 {}", basin.strong_noun()),
            Category::Two => format!("Category 2 {}", basin.strong_noun()),
            Category::Three => format!("Category 3 {}", basin.strong_noun()),
            Category::Four => format!("Category 4 {}", basin.strong_noun()),
            Category::Five => format!("Category 5 {}", basin.strong_noun()),
        }
    }
}

/// One point on the forecast track.
#[derive(Clone, Debug, PartialEq)]
pub struct ForecastPoint {
    pub position: GeoPoint,
    pub valid_time: Option<DateTime<Utc>>,
    pub max_wind_kt: Option<f32>,
    /// The 34/50/64-kt quadrant wind radii at this point, when the issuing
    /// center provides them. JTWC Tropical Cyclone Warnings carry them under
    /// each forecast time, and NHC's TCM carries them under each
    /// `FORECAST/OUTLOOK VALID` block; GDACS forecast points leave this empty.
    /// Ordered exactly as parsed (both bulletins list them strongest-first:
    /// 64, 50, 34 kt). See [`WindRadii`].
    pub wind_radii: Vec<WindRadii>,
}

/// One wind-threshold's 4-quadrant reach, the ATCF/JTWC wind-radii record: the
/// maximum radius (nautical miles) at which sustained winds of at least `kt` are
/// expected in each geographic quadrant (NE/SE/SW/NW). JTWC warnings report
/// these at 34, 50 and 64 kt for the analysis and every forecast time; a
/// threshold is omitted once the storm is below it, and a single radius given
/// with no quadrant qualifier means all four quadrants are equal. Radii are kept
/// in nautical miles exactly as the bulletin states them (1 NM = 1.852 km, see
/// [`KM_PER_NM`]).
///
/// Format reference: the ATCF Tropical Cyclone Warning / JTWC product
/// descriptions, <https://www.metoc.navy.mil/jtwc/jtwc.html>; the ATCF b-deck /
/// warning wind-radii convention (Sampson & Schrader 2000, *BAMS* 81(6),
/// "The Automated Tropical Cyclone Forecasting System (Version 3.2)",
/// doi:10.1175/1520-0477(2000)081<1231:TATCFS>2.3.CO;2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindRadii {
    /// The wind threshold this radius set describes (34, 50 or 64 kt).
    pub kt: u16,
    pub ne_nm: f32,
    pub se_nm: f32,
    pub sw_nm: f32,
    pub nw_nm: f32,
}

/// Kilometres per nautical mile (the unit JTWC/ATCF wind radii are reported in).
pub const KM_PER_NM: f32 = 1.852;

impl WindRadii {
    /// The quadrant radius (NM) toward a compass `bearing_deg` (0° = true N,
    /// clockwise). Quadrant boundaries follow the ATCF convention: NE spans
    /// [0°,90°), SE [90°,180°), SW [180°,270°), NW [270°,360°).
    pub fn radius_nm_at(&self, bearing_deg: f32) -> f32 {
        let b = bearing_deg.rem_euclid(360.0);
        if b < 90.0 {
            self.ne_nm
        } else if b < 180.0 {
            self.se_nm
        } else if b < 270.0 {
            self.sw_nm
        } else {
            self.nw_nm
        }
    }

    /// The largest of the four quadrant radii (NM). Zero when the record is
    /// empty (no quadrant carried a radius).
    pub fn max_nm(&self) -> f32 {
        self.ne_nm.max(self.se_nm).max(self.sw_nm).max(self.nw_nm)
    }
}

/// Earth radius (km) matching the app's azimuthal-equidistant projection, where
/// 1° = 111.32 km (see `ui_core::geo::aeqd_forward_km`), so a wind-radii ring
/// built here lines up exactly with `lon_lat_to_screen` after projection.
const EARTH_RADIUS_KM: f32 = 111.32 * 180.0 / std::f32::consts::PI;

/// Great-circle destination from `origin`, a `distance_km` along a compass
/// `bearing_deg` (0° = true N, clockwise). The standard spherical "direct"
/// (destination-point) formula on a sphere of radius [`EARTH_RADIUS_KM`].
pub fn destination_point(origin: GeoPoint, bearing_deg: f32, distance_km: f32) -> GeoPoint {
    let ang = distance_km / EARTH_RADIUS_KM; // angular distance (radians)
    let (phi1, lam1) = (origin.lat.to_radians(), origin.lon.to_radians());
    let theta = bearing_deg.to_radians();
    let (sin_ang, cos_ang) = ang.sin_cos();
    let (sin_phi1, cos_phi1) = phi1.sin_cos();
    let phi2 = (sin_phi1 * cos_ang + cos_phi1 * sin_ang * theta.cos())
        .clamp(-1.0, 1.0)
        .asin();
    let lam2 = lam1 + (theta.sin() * sin_ang * cos_phi1).atan2(cos_ang - sin_phi1 * phi2.sin());
    GeoPoint {
        lon: normalize_lon(lam2.to_degrees()),
        lat: phi2.to_degrees(),
    }
}

/// Build the closed geographic outline of one wind-radii threshold about
/// `center`: four quarter-circle arcs (NE/SE/SW/NW), each at its own quadrant
/// radius, joined by the short radial steps at the cardinal bearings — the
/// classic ATCF/JTWC "wind rose". `steps_per_quadrant` samples each arc (≥1);
/// the returned ring is closed (last point repeats the first). Empty when every
/// quadrant radius is zero. Pure geographic points (NM → km great-circle
/// offsets); the caller projects them with `lon_lat_to_screen`.
pub fn wind_radii_ring(
    center: GeoPoint,
    radii: &WindRadii,
    steps_per_quadrant: usize,
) -> Vec<GeoPoint> {
    if radii.max_nm() <= 0.0 {
        return Vec::new();
    }
    let steps = steps_per_quadrant.max(1);
    let mut ring = Vec::with_capacity(steps * 4 + 5);
    // (arc start bearing, quadrant radius NM); each arc sweeps 90°.
    let quads = [
        (0.0f32, radii.ne_nm),
        (90.0, radii.se_nm),
        (180.0, radii.sw_nm),
        (270.0, radii.nw_nm),
    ];
    for (start, r_nm) in quads {
        let r_km = r_nm.max(0.0) * KM_PER_NM;
        for s in 0..=steps {
            let bearing = start + 90.0 * (s as f32 / steps as f32);
            ring.push(destination_point(center, bearing, r_km));
        }
    }
    if let Some(&first) = ring.first() {
        ring.push(first);
    }
    ring
}

/// The convex hull (Andrew's monotone chain, in the lon/lat plane) of a set of
/// geographic points, returned counter-clockwise and closed (first vertex
/// repeated). Fewer than three distinct points are returned unchanged. This
/// WAS the 34-kt danger-area construction until v0.29.2: on a recurving track
/// a convex hull straight-lines from the first circle to the last, so
/// [`danger_area_34kt`] now builds the tapered [`track_circle_envelope`]
/// instead; the hull stays as a general helper and as the legacy comparator
/// in tests and the `tropical_radii_probe` example. Operating in the lon/lat
/// plane is exact enough for a single-basin storm well away from a pole or
/// the antimeridian (the West Pacific warnings this serves sit near
/// 120–150°E).
pub fn convex_hull(points: &[GeoPoint]) -> Vec<GeoPoint> {
    let mut pts: Vec<GeoPoint> = points.to_vec();
    pts.sort_by(|a, b| {
        a.lon
            .total_cmp(&b.lon)
            .then_with(|| a.lat.total_cmp(&b.lat))
    });
    pts.dedup_by(|a, b| a.lon == b.lon && a.lat == b.lat);
    if pts.len() < 3 {
        return pts;
    }
    // Cross product of OA×OB (z component); >0 ⇒ counter-clockwise turn.
    let cross = |o: GeoPoint, a: GeoPoint, b: GeoPoint| {
        (a.lon - o.lon) * (b.lat - o.lat) - (a.lat - o.lat) * (b.lon - o.lon)
    };
    let mut lower: Vec<GeoPoint> = Vec::new();
    for &p in &pts {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], p) <= 0.0 {
            lower.pop();
        }
        lower.push(p);
    }
    let mut upper: Vec<GeoPoint> = Vec::new();
    for &p in pts.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], p) <= 0.0 {
            upper.pop();
        }
        upper.push(p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    if let Some(&first) = lower.first() {
        lower.push(first);
    }
    lower
}

/// Sampling stride (km) along the envelope's straight tangent edges. The
/// edges are exactly straight, so the stride only sets how densely the
/// interior prune can trim an edge that sinks into a neighboring capsule.
const ENVELOPE_SAMPLE_KM: f32 = 30.0;
/// Arc sampling step (degrees) for vertex joins, end caps and the
/// single-circle case. 10° keeps the chord sagitta under 0.4 % of the radius.
const ENVELOPE_ARC_STEP_DEG: f32 = 10.0;
/// A boundary candidate sunk deeper than this (km) into one of the corridor's
/// own capsules is pruned — it belongs to a stretch of offset curve that a
/// neighboring, bigger/closer circle has swallowed (the inner-bend
/// "offsets cross" case). True boundary points sit at depth ≈ 0.
const ENVELOPE_PRUNE_KM: f32 = 1.0;
/// Consecutive track points closer than this (km) are merged before any
/// bearing is taken — a near-duplicate forecast position would otherwise
/// orient an offset from pure position noise.
const ENVELOPE_MIN_SEG_KM: f32 = 5.0;

/// Great-circle distance (km) between two points on the app's
/// [`EARTH_RADIUS_KM`] sphere (haversine), consistent with
/// [`destination_point`].
fn haversine_km(a: GeoPoint, b: GeoPoint) -> f32 {
    let (phi1, phi2) = (a.lat.to_radians(), b.lat.to_radians());
    let half_dphi = (phi2 - phi1) * 0.5;
    let half_dlam = (b.lon - a.lon).to_radians() * 0.5;
    let h = half_dphi.sin().powi(2) + phi1.cos() * phi2.cos() * half_dlam.sin().powi(2);
    2.0 * EARTH_RADIUS_KM * h.sqrt().min(1.0).asin()
}

/// Initial great-circle bearing (degrees, 0° = true N, clockwise) from `a`
/// toward `b` — the standard forward-azimuth formula, the exact inverse of
/// [`destination_point`] (following that bearing for their haversine distance
/// lands on `b`).
fn initial_bearing_deg(a: GeoPoint, b: GeoPoint) -> f32 {
    let (phi1, phi2) = (a.lat.to_radians(), b.lat.to_radians());
    let dlam = (b.lon - a.lon).to_radians();
    let y = dlam.sin() * phi2.cos();
    let x = phi1.cos() * phi2.sin() - phi1.sin() * phi2.cos() * dlam.cos();
    y.atan2(x).to_degrees()
}

/// Wrap an angle difference into (-180°, 180°].
fn wrap_deg_180(deg: f32) -> f32 {
    let d = deg % 360.0;
    if d > 180.0 {
        d - 360.0
    } else if d < -180.0 {
        d + 360.0
    } else {
        d
    }
}

/// Signed distance (km, negative inside) from `x` to the tapered capsule swept
/// from the circle (`a`, `ra_km`) to the circle `len_km` away along bearing
/// `theta_ab_deg` with radius `rb_km` — i.e. the union of the linearly
/// interpolated circles, which equals the convex hull of the two end circles.
/// Exact on the sphere: `x` is put into cross-track / along-track coordinates
/// about the segment geodesic and the 1-D convex minimum
/// `min_s |x − P(s)| − r(s)` is solved in closed form.
fn capsule_signed_distance_km(
    x: GeoPoint,
    a: GeoPoint,
    ra_km: f32,
    rb_km: f32,
    theta_ab_deg: f32,
    len_km: f32,
) -> f32 {
    let d_ax = haversine_km(a, x);
    if len_km <= 0.0 {
        return d_ax - ra_km.max(rb_km);
    }
    let rel = (initial_bearing_deg(a, x) - theta_ab_deg).to_radians();
    let ang = d_ax / EARTH_RADIUS_KM;
    let cross_ang = (ang.sin() * rel.sin()).clamp(-1.0, 1.0).asin();
    let cross_km = cross_ang.abs() * EARTH_RADIUS_KM;
    let cos_along = if cross_ang.cos() <= 1e-6 {
        0.0
    } else {
        (ang.cos() / cross_ang.cos()).clamp(-1.0, 1.0)
    };
    let mut along_km = cos_along.acos() * EARTH_RADIUS_KM;
    if rel.cos() < 0.0 {
        along_km = -along_km;
    }
    // Planar sub-problem in (along, cross): minimize
    // sqrt((along − s)² + cross²) − (ra + m·s) over s ∈ [0, len]. Convex, so
    // the clamped unconstrained optimum s* = along + cross·m/√(1−m²) is the
    // minimum (m > 0 pushes the closest swept circle toward the growing end).
    let m = ((rb_km - ra_km) / len_km).clamp(-0.999, 0.999);
    let s = (along_km + cross_km * m / (1.0 - m * m).sqrt()).clamp(0.0, len_km);
    (along_km - s).hypot(cross_km) - (ra_km + m * s)
}

/// Interior samples of a circular arc around `center`: from bearing
/// `from_deg`, sweeping `sweep_deg` (positive = clockwise), both endpoints
/// EXCLUDED (the caller's chain already carries them).
fn arc_interior_points(
    center: GeoPoint,
    r_km: f32,
    from_deg: f32,
    sweep_deg: f32,
) -> Vec<GeoPoint> {
    if r_km <= 0.0 {
        return Vec::new();
    }
    let steps = (sweep_deg.abs() / ENVELOPE_ARC_STEP_DEG).ceil() as usize;
    (1..steps)
        .map(|k| destination_point(center, from_deg + sweep_deg * k as f32 / steps as f32, r_km))
        .collect()
}

/// The envelope of per-point circles laid along a (curved) track — the
/// tropical-cyclone "cone" construction: a tapered buffer of the polyline
/// `(P_i, r_i)` (radii in NM) whose boundary follows every bend of the track
/// instead of straight-lining from the first circle to the last the way a
/// convex hull does.
///
/// Geometry, per segment: the exact external tangent to the two end circles —
/// the offset bearing leans `asin((r_b − r_a)/d)` back from the perpendicular,
/// so a strong taper still yields true straight tangent edges (a plain
/// perpendicular offset would sit `r·(1 − cos tilt)` inside the hull at the
/// small end). At each interior vertex the outer side of the bend walks the
/// vertex circle's arc between the two tangent contacts; on the inner side the
/// tangent edges cross, and since both are tangent to the vertex circle their
/// intersection (the miter) sits on the contact bisector at `r/cos(Δ/2)` —
/// that corner is emitted instead of a concave arc, and both adjoining edges
/// are trimmed back to it (`r·tan(Δ/2)` from their contacts) so no sample is
/// ever left past the crossing. Edge stretches that sink into a NON-adjacent
/// capsule (segments much shorter than the radii — the JTWC West-Pacific
/// regime, where 6–12-h track steps are ~150 NM under 260-NM gale radii) are
/// pruned by an exact point-in-tapered-capsule test; together the trim and
/// the prune keep the inner side of a sharp bend from self-looping. Round
/// caps close
/// both ends (a half circle widened/narrowed by the local tilt), and the ring
/// is emitted left edge forward → end cap → right edge backward → start cap,
/// closed (first point repeated).
///
/// Consecutive points closer than [`ENVELOPE_MIN_SEG_KM`], or whose circle
/// lies entirely inside a neighbor's (`d ≤ |r_a − r_b|`), are merged keeping
/// the larger circle, so duplicate forecast positions can't orient an offset
/// from noise. One surviving circle returns just that circle; an empty input
/// (or all-zero radii) returns nothing. Coordinates are geographic degrees
/// with true spherical offsets ([`destination_point`], matching
/// [`wind_radii_ring`]), valid single-basin away from the poles and the
/// antimeridian like the rest of this module's overlay geometry.
pub fn track_circle_envelope(points_nm: &[(GeoPoint, f32)]) -> Vec<GeoPoint> {
    // -- sanitize, convert to km, merge duplicate/contained neighbors --------
    let mut discs: Vec<(GeoPoint, f32)> = points_nm
        .iter()
        .filter(|(p, r)| p.lon.is_finite() && p.lat.is_finite() && r.is_finite())
        .map(|&(p, r)| (p, r.max(0.0) * KM_PER_NM))
        .collect();
    loop {
        let before = discs.len();
        let mut i = 0;
        while i + 1 < discs.len() {
            let d = haversine_km(discs[i].0, discs[i + 1].0);
            if d <= ENVELOPE_MIN_SEG_KM.max((discs[i].1 - discs[i + 1].1).abs()) {
                let keep = if discs[i + 1].1 >= discs[i].1 {
                    discs[i + 1]
                } else {
                    discs[i]
                };
                discs[i] = keep;
                discs.remove(i + 1);
            } else {
                i += 1;
            }
        }
        if discs.len() == before {
            break;
        }
    }
    match discs.as_slice() {
        [] => return Vec::new(),
        &[(center, r_km)] => {
            if r_km <= 0.0 {
                return Vec::new();
            }
            let steps = (360.0 / ENVELOPE_ARC_STEP_DEG).ceil() as usize;
            let mut ring: Vec<GeoPoint> = (0..steps)
                .map(|k| destination_point(center, 360.0 * k as f32 / steps as f32, r_km))
                .collect();
            ring.push(ring[0]);
            return ring;
        }
        _ => {}
    }
    if discs.iter().all(|(_, r)| *r <= 0.0) {
        return Vec::new(); // a zero-width corridor is not a drawable cone
    }

    // -- per-segment frame: bearing, length, external-tangent tilt -----------
    struct Seg {
        theta: f32,
        len_km: f32,
        tilt_deg: f32,
    }
    let segs: Vec<Seg> = discs
        .windows(2)
        .map(|w| {
            let len_km = haversine_km(w[0].0, w[1].0);
            let theta = initial_bearing_deg(w[0].0, w[1].0);
            // |Δr| < d after the merge pass; the clamp is a numeric guard.
            let tilt_deg = ((w[1].1 - w[0].1) / len_km)
                .clamp(-0.99, 0.99)
                .asin()
                .to_degrees();
            Seg {
                theta,
                len_km,
                tilt_deg,
            }
        })
        .collect();
    // Tangent contact bearing at a segment's endpoints: perpendicular to the
    // travel bearing, leaned back by the taper tilt (side −1 = left, +1 =
    // right of travel). Both ends of a segment share it — the radius to an
    // external tangent's contact point is parallel at both circles.
    let offset_bearing = |seg: &Seg, side: f32| seg.theta + side * (90.0 + seg.tilt_deg);

    let side_chain = |side: f32| -> Vec<GeoPoint> {
        // Contact-bearing change across interior vertex `v` on this side.
        let joint_delta = |v: usize| {
            wrap_deg_180(offset_bearing(&segs[v], side) - offset_bearing(&segs[v - 1], side))
        };
        // On an inner join the tangent edges cross at the miter, so the runs
        // on both sides must be TRIMMED back to it: the contact-to-miter
        // distance along each edge is r·tan(Δ/2). 25 % slack — over-trimming
        // is free (the bridged stretch is collinear with the tangent edge and
        // the emitted miter), while any sample left past the miter would sit
        // a sub-prune sliver inside the neighboring capsule and micro-loop
        // around the miter (found on the real Bavi #25 geometry).
        let inner_trim_km = |v: usize| -> f32 {
            let delta = joint_delta(v);
            if side * delta > 0.0 {
                let half = (delta.abs() * 0.5).min(75.0).to_radians();
                discs[v].1 * half.tan() * 1.25
            } else {
                0.0
            }
        };
        let mut chain = Vec::new();
        for (i, seg) in segs.iter().enumerate() {
            let ob = offset_bearing(seg, side);
            if i > 0 {
                let ob_prev = offset_bearing(&segs[i - 1], side);
                let delta = joint_delta(i);
                let (center, r_km) = discs[i];
                if side * delta <= 0.0 {
                    // Outer side of the bend: the vertex circle bulges out
                    // between the two tangent edges — walk its arc.
                    chain.extend(arc_interior_points(center, r_km, ob_prev, delta));
                } else if r_km > 0.0 {
                    // Inner side: the tangent edges cross. Both are tangent to
                    // the vertex circle, so the miter sits on the contact
                    // bisector at r/cos(Δ/2). The cos clamp caps a degenerate
                    // near-reversal at 4r; the interior prune below removes it
                    // if it lands inside a neighboring capsule.
                    let half = (delta.abs() * 0.5).to_radians();
                    chain.push(destination_point(
                        center,
                        ob_prev + 0.5 * delta,
                        r_km / half.cos().max(0.25),
                    ));
                }
            }
            // Tangent-edge run, trimmed to the miters at inner-joined ends
            // (the edge between the contact points spans len·cos(tilt)).
            let line_len = (seg.len_km * seg.tilt_deg.to_radians().cos()).max(1e-3);
            let u_from = if i > 0 {
                (inner_trim_km(i) / line_len).min(1.0)
            } else {
                0.0
            };
            let u_to = if i + 1 < segs.len() {
                1.0 - (inner_trim_km(i + 1) / line_len).min(1.0)
            } else {
                1.0
            };
            if u_to < u_from {
                continue; // fully swallowed by its neighbors' miters
            }
            let steps = ((u_to - u_from) * seg.len_km / ENVELOPE_SAMPLE_KM)
                .ceil()
                .max(1.0) as usize;
            let dr = discs[i + 1].1 - discs[i].1;
            for k in 0..=steps {
                let u = u_from + (u_to - u_from) * k as f32 / steps as f32;
                let on_track = destination_point(discs[i].0, seg.theta, seg.len_km * u);
                let r_km = discs[i].1 + dr * u;
                chain.push(if r_km > 0.0 {
                    destination_point(on_track, ob, r_km)
                } else {
                    on_track
                });
            }
        }
        chain
    };

    // -- assemble: left edge forward, end cap, right edge backward, start cap
    let left = side_chain(-1.0);
    let right = side_chain(1.0);
    let first_seg = &segs[0];
    let last_seg = segs.last().expect("two discs make a segment");
    let (start_center, start_r) = discs[0];
    let (end_center, end_r) = *discs.last().expect("at least two discs");
    let mut ring = left;
    ring.extend(arc_interior_points(
        end_center,
        end_r,
        offset_bearing(last_seg, -1.0),
        180.0 + 2.0 * last_seg.tilt_deg,
    ));
    ring.extend(right.iter().rev().copied());
    ring.extend(arc_interior_points(
        start_center,
        start_r,
        offset_bearing(first_seg, 1.0),
        180.0 - 2.0 * first_seg.tilt_deg,
    ));

    // -- prune candidates swallowed by a neighboring capsule, close the ring -
    let penetration_km = |x: GeoPoint| -> f32 {
        let mut worst = 0.0f32;
        for (i, seg) in segs.iter().enumerate() {
            let sd = capsule_signed_distance_km(
                x,
                discs[i].0,
                discs[i].1,
                discs[i + 1].1,
                seg.theta,
                seg.len_km,
            );
            worst = worst.max(-sd);
        }
        worst
    };
    ring.retain(|p| penetration_km(*p) <= ENVELOPE_PRUNE_KM);
    ring.dedup_by(|a, b| haversine_km(*a, *b) < 0.2);
    while ring.len() >= 2 {
        if haversine_km(ring[0], ring[ring.len() - 1]) < 0.2 {
            ring.pop();
        } else {
            break;
        }
    }
    if ring.len() < 3 {
        return Vec::new();
    }
    ring.push(ring[0]);
    ring
}

/// The 34-kt "wind danger area" (a.k.a. the USN ship-avoidance swath): a closed
/// geographic ring enclosing every 34-kt wind-radii rose along the storm's
/// track. Each point contributes a circle at its LARGEST 34-kt quadrant radius
/// (which contains the whole rose), and the ring is the tapered envelope of
/// those circles laid along the forecast polyline ([`track_circle_envelope`]),
/// so it follows a curved track. The previous construction — a convex hull of
/// the sampled roses — straight-lined from the small first circle to the big
/// last one, so on a recurving track (live report: Typhoon Bavi warning #25)
/// the sides cut the inside of the bend and the whole front of the cone drew
/// visibly too fat. Empty when no point carries a nonzero 34-kt radius. Pure
/// geographic points; the caller projects them.
pub fn danger_area_34kt<'a>(
    points: impl Iterator<Item = (GeoPoint, &'a [WindRadii])>,
) -> Vec<GeoPoint> {
    let discs: Vec<(GeoPoint, f32)> = points
        .filter_map(|(center, radii)| {
            radii
                .iter()
                .find(|r| r.kt == 34)
                .map(|r34| (center, r34.max_nm()))
        })
        .filter(|(_, r_nm)| *r_nm > 0.0)
        .collect();
    track_circle_envelope(&discs)
}

/// A storm's track/cone geometry (from GDACS `getgeometry`, or NHC GIS later).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StormGeometry {
    pub centroid: Option<GeoPoint>,
    /// Track polylines (past + forecast). GDACS delivers the track as many
    /// short, independently-oriented segments, so these are kept SEPARATE (not
    /// concatenated) — flattening them into one polyline zigzags and draws
    /// spurious connecting lines. Each inner Vec is one drawable segment.
    pub track: Vec<Vec<GeoPoint>>,
    /// Cone-of-uncertainty outer ring.
    pub cone: Vec<GeoPoint>,
    /// Official forecast track points (position + valid time + per-point max
    /// wind, where the office provides it). This is the transport that carries
    /// the parsed forecast up to [`TropicalCyclone::forecast`]; see the parsers
    /// below for exactly what each source supplies.
    pub forecast: Vec<ForecastPoint>,
    /// The 34/50/64-kt wind radii at the CURRENT (analysis) position, from a
    /// JTWC warning's `PRESENT WIND DISTRIBUTION` block or an NHC TCM's
    /// `MAX SUSTAINED WINDS` block (empty for GDACS-only storms). Lets the
    /// current-position glyph draw its wind rose and anchors the 34-kt danger
    /// area at the storm.
    pub current_wind_radii: Vec<WindRadii>,
    /// The identity + analysis vitals of the bulletin this geometry came from
    /// (JTWC warning / NHC TCM); `None` for GDACS-only storms. Mirrored onto
    /// the storm record by [`sync_storm_with_geometry`].
    pub warning: Option<WarningInfo>,
}

/// One active tropical cyclone, source-agnostic.
#[derive(Clone, Debug, PartialEq)]
pub struct TropicalCyclone {
    /// Stable id, e.g. `nhc:al012026` or `gdacs:1001279:17`.
    pub id: String,
    /// Storm name without any season suffix, e.g. `Alberto`, `Bavi`.
    pub name: String,
    pub basin: Basin,
    pub source: Source,
    /// Human label, e.g. "Category 4 Typhoon".
    pub classification: String,
    pub category: Option<Category>,
    pub position: GeoPoint,
    pub max_wind_kt: Option<f32>,
    pub gust_kt: Option<f32>,
    pub min_pressure_mb: Option<f32>,
    pub movement_dir_deg: Option<f32>,
    pub movement_speed_kt: Option<f32>,
    /// When the underlying advisory/analysis was issued.
    pub advisory_time: Option<DateTime<Utc>>,
    /// GDACS alert level ("Red"/"Orange"/"Green"), if any.
    pub alert_level: Option<String>,
    /// Land areas at risk (GDACS `country`), if any.
    pub affected_areas: Option<String>,
    pub forecast: Vec<ForecastPoint>,
    /// The 34/50/64-kt wind radii at the current position (from a matched JTWC
    /// warning's analysis block); empty otherwise. Mirrored from
    /// [`StormGeometry::current_wind_radii`] by the overlay layer.
    pub current_wind_radii: Vec<WindRadii>,
    pub cone: Vec<GeoPoint>,
    /// A human report page to open externally (never scraped).
    pub report_url: Option<String>,
    /// The source URL to fetch this storm's track/cone geometry.
    pub geometry_url: Option<String>,
    /// For a JTWC-covered GDACS storm (West Pacific, Indian Ocean, Southern
    /// Hemisphere), the JTWC Tropical Cyclone Warning text URL carrying
    /// per-point forecast intensity (`wpNNyyweb.txt`). Set by matching the JTWC
    /// RSS feed to this storm's name; enriches the GDACS getgeometry track/cone
    /// with real per-point max wind. `None` when no active JTWC warning matches.
    pub forecast_url: Option<String>,
    /// The identity + analysis vitals of the official bulletin behind this
    /// storm's forecast (JTWC warning / NHC TCM), mirrored from the fetched
    /// geometry by [`sync_storm_with_geometry`]. The card renders it so the
    /// user can always see which warning the intensity came from and how old
    /// it is.
    pub warning: Option<WarningInfo>,
    /// The newest JTWC warning number the RSS feed advertises for this storm
    /// (`Warning #25`), when one is active. The UI's geometry cache compares
    /// it against the number it fetched under, so a re-issued JTWC warning
    /// triggers a refetch exactly like a newer NHC advisory time does.
    pub jtwc_warning_nr: Option<u32>,
}

impl TropicalCyclone {
    pub fn max_wind_mph(&self) -> Option<f32> {
        self.max_wind_kt.map(|kt| kt / KT_PER_MPH)
    }

    pub fn max_wind_kmh(&self) -> Option<f32> {
        self.max_wind_kt.map(|kt| kt / KT_PER_KMH)
    }

    /// Max sustained wind across the units meteorologists read, e.g.
    /// "145 kt · 167 mph · 269 km/h". None when wind is unknown.
    pub fn wind_summary(&self) -> Option<String> {
        let kt = self.max_wind_kt?;
        Some(format!(
            "{:.0} kt · {:.0} mph · {:.0} km/h",
            kt,
            kt / KT_PER_MPH,
            kt / KT_PER_KMH
        ))
    }

    /// Minimum central pressure, e.g. "965 mb".
    pub fn pressure_summary(&self) -> Option<String> {
        self.min_pressure_mb.map(|mb| format!("{mb:.0} mb"))
    }

    /// Motion toward a heading, e.g. "NNW (340°) at 12 kt". None when either
    /// component is unknown.
    pub fn motion_summary(&self) -> Option<String> {
        let dir = self.movement_dir_deg?;
        let speed = self.movement_speed_kt?;
        Some(format!(
            "{} ({:.0}°) at {:.0} kt",
            compass_16(dir),
            dir,
            speed
        ))
    }
}

/// 16-point compass label for a bearing in degrees (0° = N, clockwise).
pub fn compass_16(deg: f32) -> &'static str {
    const POINTS: [&str; 16] = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
        "NW", "NNW",
    ];
    let idx = ((deg.rem_euclid(360.0) / 22.5).round() as usize) % 16;
    POINTS[idx]
}

pub const KT_PER_KMH: f32 = 0.539_957;
pub const KT_PER_MPH: f32 = 0.868_976;

fn normalize_lon(lon: f32) -> f32 {
    let mut l = lon % 360.0;
    if l > 180.0 {
        l -= 360.0;
    } else if l < -180.0 {
        l += 360.0;
    }
    l
}

/// Parse a timestamp that may be RFC 3339 (`...Z`) or a naive ISO string
/// (GDACS, implicitly UTC).
fn parse_time(raw: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Some(dt.with_timezone(&Utc));
    }
    NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|naive| naive.and_utc())
}

/// GDACS event names carry a season suffix ("BAVI-26"); NHC names are clean.
/// Return a title-cased bare name.
fn clean_storm_name(raw: &str) -> String {
    let bare = raw
        .trim()
        .trim_start_matches("Tropical Cyclone ")
        .split('-')
        .next()
        .unwrap_or(raw)
        .trim();
    title_case(bare)
}

fn title_case(s: &str) -> String {
    s.split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// NHC CurrentStorms.json
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NhcCurrentStorms {
    #[serde(default)]
    active_storms: Vec<NhcStorm>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NhcStorm {
    id: String,
    name: String,
    #[serde(default)]
    classification: String,
    #[serde(default)]
    intensity: String,
    #[serde(default)]
    pressure: String,
    latitude_numeric: f32,
    longitude_numeric: f32,
    #[serde(default)]
    movement_dir: Option<f32>,
    #[serde(default)]
    movement_speed: Option<f32>,
    #[serde(default)]
    last_update: Option<String>,
    #[serde(default)]
    public_advisory: Option<NhcProduct>,
    /// The Tropical Cyclone Forecast/Advisory (TCM) product — the machine-
    /// readable forecast track WITH per-point max wind. Its `url` is a stable
    /// "latest" bulletin, e.g. `.../text/MIATCMAT1.shtml`.
    #[serde(default)]
    forecast_advisory: Option<NhcProduct>,
}

#[derive(Deserialize)]
struct NhcProduct {
    #[serde(default)]
    url: Option<String>,
}

/// NHC classification codes → whether the wind label should override.
fn nhc_basin(id: &str) -> Basin {
    match id.get(0..2) {
        Some("al") | Some("AL") => Basin::Atlantic,
        Some("ep") | Some("EP") => Basin::EastPacific,
        Some("cp") | Some("CP") => Basin::CentralPacific,
        _ => Basin::Atlantic,
    }
}

/// Parse NHC's `CurrentStorms.json` into unified records.
pub fn parse_nhc_current_storms(json: &str) -> Result<Vec<TropicalCyclone>, String> {
    let parsed: NhcCurrentStorms =
        serde_json::from_str(json).map_err(|err| format!("NHC parse: {err}"))?;
    Ok(parsed
        .active_storms
        .into_iter()
        .map(nhc_to_cyclone)
        .collect())
}

fn nhc_to_cyclone(storm: NhcStorm) -> TropicalCyclone {
    let basin = nhc_basin(&storm.id);
    let max_wind_kt = storm.intensity.trim().parse::<f32>().ok();
    let category = max_wind_kt.map(Category::from_wind_kt);
    let classification = category
        .map(|category| category.label(basin))
        .unwrap_or_else(|| nhc_classification_label(&storm.classification));
    TropicalCyclone {
        id: format!("nhc:{}", storm.id),
        name: title_case(storm.name.trim()),
        basin,
        source: Source::Nhc,
        classification,
        category,
        position: GeoPoint {
            lon: storm.longitude_numeric,
            lat: storm.latitude_numeric,
        },
        max_wind_kt,
        gust_kt: None,
        min_pressure_mb: storm.pressure.trim().parse::<f32>().ok(),
        movement_dir_deg: storm.movement_dir,
        movement_speed_kt: storm.movement_speed,
        advisory_time: storm.last_update.as_deref().and_then(parse_time),
        alert_level: None,
        affected_areas: None,
        forecast: Vec::new(),
        current_wind_radii: Vec::new(),
        cone: Vec::new(),
        report_url: storm.public_advisory.and_then(|advisory| advisory.url),
        // The forecast-advisory (TCM) URL is fetched on the second pass and
        // parsed into `forecast` by `parse_nhc_forecast_advisory`.
        geometry_url: storm.forecast_advisory.and_then(|advisory| advisory.url),
        // NHC's own TCM already carries per-point intensity via geometry_url;
        // no separate JTWC enrichment needed for NHC basins.
        forecast_url: None,
        // Attached from the fetched TCM by sync_storm_with_geometry.
        warning: None,
        jtwc_warning_nr: None,
    }
}

fn nhc_classification_label(code: &str) -> String {
    match code.trim() {
        "TD" => "Tropical Depression",
        "TS" => "Tropical Storm",
        "HU" => "Hurricane",
        "MH" => "Major Hurricane",
        "PTC" => "Potential Tropical Cyclone",
        "STD" => "Subtropical Depression",
        "STS" => "Subtropical Storm",
        other if !other.is_empty() => return other.to_owned(),
        _ => "Tropical Cyclone",
    }
    .to_owned()
}

// ---------------------------------------------------------------------------
// NHC Tropical Cyclone Forecast/Advisory (TCM) — per-point forecast + intensity
// ---------------------------------------------------------------------------

/// Parse the official forecast track from an NHC Tropical Cyclone
/// Forecast/Advisory (a.k.a. TCM / "FORECAST/ADVISORY", WMO header e.g.
/// `MIATCMAT4`). Each forecast point is a `FORECAST VALID`/`OUTLOOK VALID` line
/// (`DD/HHMMZ  LATn  LONw`) followed by a `MAX WIND nnn KT` line and the
/// `34/50/64 KT... nnNE nnSE nnSW nnNW.` quadrant wind-radii lines, so NHC
/// carries per-point **valid time, position, max sustained wind (kt), and
/// 34/50/64-kt quadrant wind radii** — everything the Saffir–Simpson color
/// ramp and the wind-rose / 34-kt danger-area rendering need.
///
/// This is NHC's machine-readable forecast product (linked from
/// `CurrentStorms.json` as `forecastAdvisory.url`); we parse the fixed columnar
/// bulletin text, never the human advisory web page. Product/format reference:
/// NHC "Tropical Cyclone Forecast/Advisory" description,
/// <https://www.nhc.noaa.gov/help/tcm.shtml>; the quadrant wind-radii record
/// follows the ATCF convention (Sampson & Schrader 2000, *BAMS* 81(6),
/// doi:10.1175/1520-0477(2000)081<1231:TATCFS>2.3.CO;2).
pub fn parse_nhc_forecast_advisory(text: &str) -> Vec<ForecastPoint> {
    let issued = nhc_issuance_date(text);
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (i, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        let Some(rest) = line
            .strip_prefix("FORECAST VALID ")
            .or_else(|| line.strip_prefix("OUTLOOK VALID "))
        else {
            continue;
        };
        let Some((position, valid_time)) = parse_nhc_valid_line(rest, issued) else {
            continue;
        };
        // This point's block runs to the next blank line or the next
        // FORECAST/OUTLOOK header, so one block's wind + radii can never bleed
        // into the next (the TCM separates every block with a blank line).
        let block = nhc_block_after(&lines, i);
        let max_wind_kt = block.iter().find_map(|l| parse_nhc_max_wind(l.trim()));
        let wind_radii = block
            .iter()
            .filter_map(|l| parse_nhc_radii_line(l))
            .collect();
        out.push(ForecastPoint {
            position,
            valid_time,
            max_wind_kt,
            wind_radii,
        });
    }
    out
}

/// The lines belonging to the block that starts after line `i`: everything up
/// to (excluding) the first blank line or the next `FORECAST VALID` /
/// `OUTLOOK VALID` header.
fn nhc_block_after<'a>(lines: &'a [&'a str], i: usize) -> &'a [&'a str] {
    let end = lines[i + 1..]
        .iter()
        .position(|l| {
            let t = l.trim();
            t.is_empty() || t.starts_with("FORECAST VALID") || t.starts_with("OUTLOOK VALID")
        })
        .map(|offset| i + 1 + offset)
        .unwrap_or(lines.len());
    &lines[i + 1..end]
}

/// Parse the current-position (analysis) wind radii from an NHC TCM — the
/// `34/50/64 KT` quadrant lines directly under `MAX SUSTAINED WINDS`, valid at
/// the advisory's `CENTER LOCATED NEAR` position. Empty when the storm carries
/// no radii (below 34 kt). The block is bounded at the first blank line, and
/// [`parse_nhc_radii_line`] gates on the 34/50/64-kt thresholds, so the
/// adjacent `12 FT SEAS..` line (same columnar shape, different quantity) is
/// never mistaken for wind radii.
pub fn parse_nhc_current_radii(text: &str) -> Vec<WindRadii> {
    let lines: Vec<&str> = text.lines().collect();
    let Some(start) = lines
        .iter()
        .position(|l| l.trim().starts_with("MAX SUSTAINED WINDS"))
    else {
        return Vec::new();
    };
    nhc_block_after(&lines, start)
        .iter()
        .filter_map(|l| parse_nhc_radii_line(l))
        .collect()
}

/// Parse the advisory's identity + analysis vitals from an NHC TCM: the
/// `FORECAST/ADVISORY NUMBER`, the issuance instant (`2100 UTC TUE OCT 08
/// 2024`), the analysis position/time (`CENTER LOCATED NEAR 22.7N 87.5W AT
/// 08/2100Z`), the current `MAX SUSTAINED WINDS ... GUSTS`, and the
/// `ESTIMATED MINIMUM CENTRAL PRESSURE`. This is what the storm card shows so
/// the forecast's age is visible; the per-point track stays in
/// [`parse_nhc_forecast_advisory`]. `None` when the text carries none of it.
pub fn parse_nhc_warning_info(text: &str) -> Option<WarningInfo> {
    let issued_date = nhc_issuance_date(text);
    let issued = nhc_issuance_stamp(text).and_then(|(hhmm, year, month, day)| {
        let date = NaiveDate::from_ymd_opt(year, month, day)?;
        let time = NaiveTime::from_hms_opt(hhmm / 100, hhmm % 100, 0)?;
        Some(date.and_time(time).and_utc())
    });

    // "HURRICANE MILTON FORECAST/ADVISORY NUMBER  15" (specials keep the
    // phrase; a lettered intermediate number simply fails the parse → None).
    let number = text.lines().find_map(|line| {
        let at = line.find("FORECAST/ADVISORY NUMBER")?;
        line[at + "FORECAST/ADVISORY NUMBER".len()..]
            .split_whitespace()
            .next()?
            .parse::<u32>()
            .ok()
    });

    // "HURRICANE CENTER LOCATED NEAR 22.7N  87.5W AT 08/2100Z" (first one; the
    // later REPEAT... line restates the same fix).
    let center = text.lines().find_map(|line| {
        let at = line.find("CENTER LOCATED NEAR")?;
        parse_nhc_center_rest(&line[at + "CENTER LOCATED NEAR".len()..], issued_date)
    });
    let (position, position_time) = match center {
        Some((point, time)) => (Some(point), time),
        None => (None, None),
    };

    // "MAX SUSTAINED WINDS 145 KT WITH GUSTS TO 175 KT." — the first such
    // line that carries a number (guards against digit-less headers, as in
    // the JTWC parser).
    let vitals = text
        .lines()
        .map(str::trim)
        .find(|line| parse_jtwc_max_wind(line).is_some());
    let max_wind_kt = vitals.and_then(parse_jtwc_max_wind);
    let gust_kt = vitals.and_then(parse_gusts);
    let min_pressure_mb = parse_min_pressure(text);

    if number.is_none() && issued.is_none() && max_wind_kt.is_none() {
        return None;
    }
    Some(WarningInfo {
        agency: WarningAgency::Nhc,
        number,
        issued,
        position_time,
        position,
        max_wind_kt,
        gust_kt,
        // CurrentStorms.json already carries NHC motion; not parsed here.
        movement_dir_deg: None,
        movement_speed_kt: None,
        min_pressure_mb,
    })
}

/// ` 22.7N  87.5W AT 08/2100Z` (the tail of a `CENTER LOCATED NEAR` line) →
/// the analysis position and time.
fn parse_nhc_center_rest(
    rest: &str,
    issued: Option<(i32, u32, u32)>,
) -> Option<(GeoPoint, Option<DateTime<Utc>>)> {
    let mut toks = rest.split_whitespace();
    let lat = parse_signed_coord(toks.next()?, 'N', 'S')?;
    let lon = parse_signed_coord(toks.next()?, 'E', 'W')?;
    let time = toks
        .find(|tok| *tok == "AT")
        .and_then(|_| toks.next())
        .and_then(|tok| parse_tcm_time(tok, issued));
    Some((GeoPoint { lon, lat }, time))
}

/// Parse one TCM quadrant wind-radii line, e.g. `64 KT....... 25NE  25SE  25SW
/// 25NW.` (analysis block) or `34 KT...100NE 100SE  80SW 120NW.` (forecast
/// block — note the dots can abut the first radius with no space). Radii are
/// nautical miles; a `0` radius means no reach in that quadrant. None unless
/// the threshold is one of the ATCF 34/50/64-kt set AND all four quadrants are
/// present — which also rejects `MAX WIND 145 KT...` and `12 FT SEAS..` lines.
fn parse_nhc_radii_line(line: &str) -> Option<WindRadii> {
    let (kt_tok, rest) = line.trim().split_once(char::is_whitespace)?;
    let kt = kt_tok.parse::<u16>().ok()?;
    if !matches!(kt, 34 | 50 | 64) {
        return None;
    }
    let quads = rest
        .trim_start()
        .strip_prefix("KT")?
        .trim_start_matches('.');
    let (mut ne, mut se, mut sw, mut nw) = (None, None, None, None);
    for tok in quads.split_whitespace() {
        let tok = tok.trim_end_matches('.');
        let Some(at) = tok.len().checked_sub(2) else {
            continue;
        };
        let (value, quadrant) = tok.split_at(at);
        let Ok(nm) = value.trim().parse::<f32>() else {
            continue;
        };
        match quadrant {
            "NE" => ne = Some(nm),
            "SE" => se = Some(nm),
            "SW" => sw = Some(nm),
            "NW" => nw = Some(nm),
            _ => {}
        }
    }
    Some(WindRadii {
        kt,
        ne_nm: ne?,
        se_nm: se?,
        sw_nm: sw?,
        nw_nm: nw?,
    })
}

/// Pull `(HHMM, year, month, day)` from the TCM datestamp line, e.g.
/// `2100 UTC TUE OCT 08 2024` — the advisory's issuance instant.
fn nhc_issuance_stamp(text: &str) -> Option<(u32, i32, u32, u32)> {
    for line in text.lines() {
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.len() < 6 || toks[1] != "UTC" {
            continue;
        }
        let hhmm = toks[0].parse::<u32>();
        let month = month_from_abbrev(toks[toks.len() - 3]);
        let day = toks[toks.len() - 2].parse::<u32>();
        let year = toks[toks.len() - 1].parse::<i32>();
        if let (Ok(hhmm), Some(month), Ok(day), Ok(year)) = (hhmm, month, day, year) {
            return Some((hhmm, year, month, day));
        }
    }
    None
}

/// The issuance `(year, month, day)` from the TCM datestamp line. The forecast
/// lines carry only a day-of-month, so this reference resolves the real
/// month/year across month/year rollover.
fn nhc_issuance_date(text: &str) -> Option<(i32, u32, u32)> {
    nhc_issuance_stamp(text).map(|(_, year, month, day)| (year, month, day))
}

fn month_from_abbrev(abbrev: &str) -> Option<u32> {
    Some(match abbrev.to_ascii_uppercase().as_str() {
        "JAN" => 1,
        "FEB" => 2,
        "MAR" => 3,
        "APR" => 4,
        "MAY" => 5,
        "JUN" => 6,
        "JUL" => 7,
        "AUG" => 8,
        "SEP" => 9,
        "OCT" => 10,
        "NOV" => 11,
        "DEC" => 12,
        _ => return None,
    })
}

/// Parse `09/0600Z 23.8N  86.4W` (optionally `...INLAND` / `...OVER WATER`).
fn parse_nhc_valid_line(
    rest: &str,
    issued: Option<(i32, u32, u32)>,
) -> Option<(GeoPoint, Option<DateTime<Utc>>)> {
    let mut toks = rest.split_whitespace();
    let time_tok = toks.next()?;
    let lat_tok = toks.next()?;
    let lon_tok = toks.next()?;
    let lat = parse_signed_coord(lat_tok, 'N', 'S')?;
    // A trailing "...INLAND"/"...POST-TROP" rides on the longitude token.
    let lon_clean = lon_tok.split("...").next().unwrap_or(lon_tok);
    let lon = parse_signed_coord(lon_clean, 'E', 'W')?;
    let valid_time = parse_tcm_time(time_tok, issued);
    Some((GeoPoint { lon, lat }, valid_time))
}

/// `23.8N` -> +23.8, `86.4W` -> -86.4 (direction is the trailing letter).
fn parse_signed_coord(tok: &str, positive: char, negative: char) -> Option<f32> {
    let dir = tok.chars().last()?;
    let magnitude: f32 = tok[..tok.len() - dir.len_utf8()].parse().ok()?;
    if dir == positive {
        Some(magnitude)
    } else if dir == negative {
        Some(-magnitude)
    } else {
        None
    }
}

/// `09/0600Z` + the issuance `(year, month, day)` -> a UTC instant. TCM forecast
/// times only ever run FORWARD from issuance (out to day 5), so a day-of-month
/// below the issuance day means the track crossed into the next month/year.
fn parse_tcm_time(tok: &str, issued: Option<(i32, u32, u32)>) -> Option<DateTime<Utc>> {
    let stamp = tok.trim_end_matches('Z');
    let (day_s, hhmm_s) = stamp.split_once('/')?;
    resolve_forward_time(day_s.parse().ok()?, hhmm_s.parse().ok()?, issued)
}

/// Resolve a `(day-of-month, HHMM)` stamp against the issuance `(year, month,
/// day)`, given that official forecast valid times only ever run FORWARD from
/// issuance (out to ~day 5). A day-of-month below the issuance day therefore
/// means the track crossed into the next month (and year, at a Dec boundary).
/// Shared by the NHC TCM (`DD/HHMMZ`) and JTWC warning (`DDHHMMZ`) parsers.
fn resolve_forward_time(
    day: u32,
    hhmm: u32,
    issued: Option<(i32, u32, u32)>,
) -> Option<DateTime<Utc>> {
    let (year, month, issue_day) = issued?;
    let (mut y, mut m) = (year, month);
    if day < issue_day {
        if m == 12 {
            m = 1;
            y += 1;
        } else {
            m += 1;
        }
    }
    let date = NaiveDate::from_ymd_opt(y, m, day)?;
    let time = NaiveTime::from_hms_opt(hhmm / 100, hhmm % 100, 0)?;
    Some(date.and_time(time).and_utc())
}

/// `MAX WIND 145 KT...GUSTS 175 KT.` -> `145`.
fn parse_nhc_max_wind(line: &str) -> Option<f32> {
    line.strip_prefix("MAX WIND")?
        .split_whitespace()
        .next()?
        .parse::<f32>()
        .ok()
}

// ---------------------------------------------------------------------------
// JTWC — RSS discovery + Tropical Cyclone Warning (per-point forecast + wind)
// ---------------------------------------------------------------------------

/// The JTWC public RSS feed listing active Tropical Cyclone Warnings and the
/// URLs of their text/graphic products (keyless, no auth). See
/// <https://www.metoc.navy.mil/jtwc/jtwc.html>.
pub const JTWC_RSS_URL: &str = "https://www.metoc.navy.mil/jtwc/rss/jtwc.rss?tc";

/// One active JTWC warning discovered from the RSS feed: its designation
/// (e.g. `09W`), storm name (e.g. `Bavi`), the Tropical Cyclone Warning
/// text URL (`wpNNyyweb.txt`) carrying the per-point forecast + intensity, and
/// the warning sequence number the feed advertises (`Warning #25`) — the
/// freshness signal that tells the geometry cache a re-issued warning exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JtwcWarningRef {
    pub designation: String,
    pub name: String,
    pub warning_url: String,
    pub warning_nr: Option<u32>,
}

/// Parse the JTWC RSS feed into the list of active Tropical Cyclone Warnings.
///
/// Each active storm appears as a bold header `<... NNX (Name) Warning #NN ...>`
/// followed by a `TC Warning Text` link to its `{basin}{NN}{yy}web.txt` product.
/// We pair each storm-warning URL with the storm header that immediately
/// precedes it. Non-storm `web.txt` products (the basin-wide "Significant
/// Tropical Weather Advisory" outlooks `abpwweb.txt`/`abioweb.txt`, which have
/// no storm number) are rejected by [`is_jtwc_warning_url`].
pub fn parse_jtwc_rss(xml: &str) -> Vec<JtwcWarningRef> {
    let mut out: Vec<JtwcWarningRef> = Vec::new();
    let needle = "web.txt";
    let mut cursor = 0;
    while let Some(rel) = xml[cursor..].find(needle) {
        let end = cursor + rel + needle.len();
        cursor = end;
        // The URL runs from the last `http` before the match to the end of
        // `web.txt` (RSS hrefs are absolute).
        let Some(start) = xml[..end].rfind("http") else {
            continue;
        };
        let url = &xml[start..end];
        if !is_jtwc_warning_url(url) {
            continue;
        }
        if let Some((designation, name, warning_nr)) = last_jtwc_designation(&xml[..start]) {
            out.push(JtwcWarningRef {
                designation,
                name,
                warning_url: url.to_owned(),
                warning_nr,
            });
        }
    }
    out
}

/// A storm-warning product URL ends in `{2 letters}{2-digit storm}{2-digit
/// year}web.txt` (e.g. `wp0926web.txt`). The basin-wide outlook products
/// (`abpwweb.txt`, `abioweb.txt`) have no numeric storm/year and are excluded.
fn is_jtwc_warning_url(url: &str) -> bool {
    let file = url.rsplit('/').next().unwrap_or(url);
    let Some(stem) = file.strip_suffix("web.txt") else {
        return false;
    };
    let bytes = stem.as_bytes();
    bytes.len() == 6
        && bytes[..2].iter().all(u8::is_ascii_alphabetic)
        && bytes[2..].iter().all(u8::is_ascii_digit)
}

/// The last `NNX (Name)` designation+name in `before` (the text preceding a
/// warning URL) — the header of the storm that URL belongs to — plus the
/// `Warning #NN` sequence number that follows the name in the same header.
/// `NNX` is two digits and one or more letters (`09W`, `01B`, `02S`); the name
/// is in the following parentheses. Rejects non-storm parentheticals like
/// `(JTWC CDO)` or `(Western/South Pacific Ocean)`.
fn last_jtwc_designation(before: &str) -> Option<(String, String, Option<u32>)> {
    let mut found = None;
    for (i, _) in before.match_indices('(') {
        let after = &before[i + 1..];
        let Some(close) = after.find(')') else {
            continue;
        };
        let name = after[..close].trim();
        if name.is_empty() || name.len() > 20 || !name.chars().all(|c| c.is_alphanumeric()) {
            continue;
        }
        let designation = before[..i]
            .trim_end()
            .rsplit(char::is_whitespace)
            .next()
            .unwrap_or_default();
        if is_jtwc_designation(designation) {
            let warning_nr = warning_number_after(&after[close + 1..]);
            found = Some((designation.to_owned(), title_case(name), warning_nr));
        }
    }
    found
}

/// The `#NN` warning number directly after a storm header's name — e.g.
/// `... 09W (Bavi) Warning #25 </b>` → 25. Bounded to the next few characters
/// so an unrelated `#` further down the feed is never picked up.
fn warning_number_after(text: &str) -> Option<u32> {
    let window: String = text.chars().take(40).collect();
    let hash = window.find('#')?;
    let digits: String = window[hash + 1..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// `09W`, `01B`, `02S` — two ASCII digits followed by one or more letters.
fn is_jtwc_designation(tok: &str) -> bool {
    let bytes = tok.as_bytes();
    bytes.len() >= 3
        && bytes[..2].iter().all(u8::is_ascii_digit)
        && bytes[2..].iter().all(u8::is_ascii_uppercase)
}

/// Set `forecast_url` (and the RSS-advertised warning number) on each GDACS
/// storm whose name matches an active JTWC warning, so the geometry pipeline
/// can enrich it with per-point intensity — and refetch it when the warning
/// number advances. (NHC storms already carry per-point wind in their own TCM.)
pub fn attach_jtwc_forecast_urls(storms: &mut [TropicalCyclone], refs: &[JtwcWarningRef]) {
    for storm in storms.iter_mut() {
        if storm.source != Source::Gdacs || !storm.basin.uses_jtwc_forecasts() {
            continue;
        }
        if let Some(matched) = refs
            .iter()
            .find(|r| r.name.eq_ignore_ascii_case(&storm.name) && !storm.name.is_empty())
        {
            storm.forecast_url = Some(matched.warning_url.clone());
            storm.jtwc_warning_nr = matched.warning_nr;
        }
    }
}

/// Parse the official forecast track from a JTWC Tropical Cyclone Warning
/// (`wpNNyyweb.txt`) — the West-Pacific/Indian-Ocean analogue of NHC's TCM.
/// Under `FORECASTS:` (and its `EXTENDED`/`LONG RANGE OUTLOOK` continuations)
/// each point is a `NN HRS, VALID AT:` header, then a `DDHHMMZ --- LATn LONe`
/// position line, a `MAX SUSTAINED WINDS - nnn KT` line, then the `RADIUS OF
/// 034/050/064 KT WINDS - ...` quadrant wind-radii blocks — carrying per-point
/// **valid time, position, max sustained wind (kt), and 34/50/64-kt quadrant
/// wind radii**, exactly what the Saffir–Simpson color ramp and the JTWC
/// wind-rose / danger-area rendering need. The current `WARNING POSITION`
/// (analysis) point is intentionally excluded here (only forecast points are
/// returned); its radii come from [`parse_jtwc_current_radii`]. Format
/// reference: JTWC product descriptions,
/// <https://www.metoc.navy.mil/jtwc/jtwc.html>, and the ATCF warning wind-radii
/// convention (Sampson & Schrader 2000, *BAMS* 81(6), 1231–1240).
pub fn parse_jtwc_forecast_warning(text: &str) -> Vec<ForecastPoint> {
    let issued = jtwc_issuance_date(text);
    let lines: Vec<&str> = text.lines().collect();
    // Each forecast point is a block that opens with a "NN HRS, VALID AT:"
    // header and runs until the next such header (or the trailing REMARKS
    // narrative). Slicing on the headers keeps every block's position, wind and
    // wind radii self-contained — one block never reads the next block's radii.
    let headers: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim().ends_with("HRS, VALID AT:"))
        .map(|(i, _)| i)
        .collect();
    // The last forecast block ends at the standalone "REMARKS:" line so it does
    // not swallow the free-text narrative (which itself mentions "... NM ...").
    let forecast_end = headers
        .last()
        .and_then(|&last| {
            lines
                .iter()
                .enumerate()
                .skip(last + 1)
                .find(|(_, l)| l.trim() == "REMARKS:")
                .map(|(i, _)| i)
        })
        .unwrap_or(lines.len());

    let mut out = Vec::new();
    for (k, &start) in headers.iter().enumerate() {
        let end = headers.get(k + 1).copied().unwrap_or(forecast_end);
        let block = &lines[start + 1..end];
        // Position line is the first `DDHHMMZ --- lat lon` in the block.
        let Some((position, valid_time)) = block
            .iter()
            .find_map(|l| parse_jtwc_valid_line(l.trim(), issued))
        else {
            continue;
        };
        // Intensity is the block's first `MAX SUSTAINED WINDS` line.
        let max_wind_kt = block.iter().find_map(|l| parse_jtwc_max_wind(l.trim()));
        // The 34/50/64-kt quadrant wind radii under this forecast time.
        let wind_radii = parse_wind_radii_lines(block);
        out.push(ForecastPoint {
            position,
            valid_time,
            max_wind_kt,
            wind_radii,
        });
    }
    out
}

/// Parse the current-position (analysis) wind radii from a JTWC warning's
/// `PRESENT WIND DISTRIBUTION` block — the 34/50/64-kt radii valid at the
/// `WARNING POSITION`. Empty when the block is absent (below 34 kt, or an older
/// bulletin without it). Bounded to the analysis block (up to `FORECASTS:` /
/// the first forecast header) so the forecast blocks' radii are not folded in.
/// See [`parse_jtwc_forecast_warning`] for the per-forecast-time radii.
pub fn parse_jtwc_current_radii(text: &str) -> Vec<WindRadii> {
    let lines: Vec<&str> = text.lines().collect();
    let Some(start) = lines
        .iter()
        .position(|l| l.trim().starts_with("PRESENT WIND DISTRIBUTION"))
    else {
        return Vec::new();
    };
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, l)| {
            let t = l.trim();
            t == "FORECASTS:" || t.ends_with("HRS, VALID AT:")
        })
        .map(|(i, _)| i)
        .unwrap_or(lines.len());
    parse_wind_radii_lines(&lines[start + 1..end])
}

/// A compass quadrant a wind-radii value applies to. `All` is the ATCF
/// single-radius form: a radius given with no quadrant qualifier is symmetric.
#[derive(Clone, Copy)]
enum Quadrant {
    Ne,
    Se,
    Sw,
    Nw,
    All,
}

/// The `kt` threshold of a `RADIUS OF nnn KT WINDS ...` header line, else None.
fn radius_header_kt(line: &str) -> Option<u16> {
    // "RADIUS OF 064 KT WINDS - ..." → the first token after the prefix is the
    // threshold (leading zeros parse fine).
    line.trim()
        .strip_prefix("RADIUS OF")?
        .split_whitespace()
        .next()?
        .parse::<u16>()
        .ok()
}

/// Parse `... nnn NM <QUADRANT>` on a wind-radii line into `(radius_nm,
/// quadrant)`. The radius is the number immediately BEFORE the `NM` token (so a
/// header line's leading `064 KT` threshold is never mistaken for the radius);
/// the quadrant is the word after `NM` (absent ⇒ the symmetric single-radius
/// form). None when the line carries no `NM` radius.
fn parse_quadrant_radius(line: &str) -> Option<(f32, Quadrant)> {
    let toks: Vec<&str> = line.split_whitespace().collect();
    let nm_at = toks.iter().position(|t| *t == "NM")?;
    let radius = toks.get(nm_at.checked_sub(1)?)?.parse::<f32>().ok()?;
    let quadrant = match toks.get(nm_at + 1).map(|t| t.to_ascii_uppercase()) {
        Some(q) if q.starts_with("NORTHEAST") => Quadrant::Ne,
        Some(q) if q.starts_with("SOUTHEAST") => Quadrant::Se,
        Some(q) if q.starts_with("SOUTHWEST") => Quadrant::Sw,
        Some(q) if q.starts_with("NORTHWEST") => Quadrant::Nw,
        _ => Quadrant::All,
    };
    Some((radius, quadrant))
}

/// Parse a slice of JTWC warning lines into its wind-radii thresholds. A
/// `RADIUS OF nnn KT WINDS - rrr NM <QUADRANT>` header opens a new threshold;
/// its own radius plus the following three quadrant lines fill NE/SE/SW/NW (a
/// single symmetric radius fills all four). Lines outside a `RADIUS OF` group
/// carry no quadrant radius and are ignored, so headers, `VECTOR TO ...`, and
/// separators are skipped. The slice MUST be scoped to one point's block (the
/// callers bound it), so an unrelated "... NM ..." elsewhere cannot bleed in.
fn parse_wind_radii_lines(lines: &[&str]) -> Vec<WindRadii> {
    let mut out: Vec<WindRadii> = Vec::new();
    for raw in lines {
        let line = raw.trim();
        if let Some(kt) = radius_header_kt(line) {
            out.push(WindRadii {
                kt,
                ne_nm: 0.0,
                se_nm: 0.0,
                sw_nm: 0.0,
                nw_nm: 0.0,
            });
        }
        if let Some(current) = out.last_mut()
            && let Some((nm, quadrant)) = parse_quadrant_radius(line)
        {
            match quadrant {
                Quadrant::Ne => current.ne_nm = nm,
                Quadrant::Se => current.se_nm = nm,
                Quadrant::Sw => current.sw_nm = nm,
                Quadrant::Nw => current.nw_nm = nm,
                Quadrant::All => {
                    current.ne_nm = nm;
                    current.se_nm = nm;
                    current.sw_nm = nm;
                    current.nw_nm = nm;
                }
            }
        }
    }
    out
}

/// Pull the issuance `(year, month, day)` from a JTWC warning's `DDMONYY`
/// datestamp (e.g. `06JUL26` in the REMARKS block). The forecast lines carry
/// only a day-of-month, so this reference resolves month/year across rollover.
fn jtwc_issuance_date(text: &str) -> Option<(i32, u32, u32)> {
    for tok in text.split_whitespace() {
        let t = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if t.len() != 7 || !t.is_ascii() {
            continue;
        }
        let day = t[0..2].parse::<u32>().ok();
        let month = month_from_abbrev(&t[2..5]);
        let yy = t[5..7].parse::<i32>().ok();
        if let (Some(day), Some(month), Some(yy)) = (day, month, yy) {
            return Some((2000 + yy, month, day));
        }
    }
    None
}

/// Parse a JTWC forecast position line: `061200Z --- 15.1N 142.5E` (the
/// `WARNING POSITION` variant `060000Z --- NEAR 14.3N 145.0E` is also handled).
/// West-Pacific longitudes are E (positive).
fn parse_jtwc_valid_line(
    line: &str,
    issued: Option<(i32, u32, u32)>,
) -> Option<(GeoPoint, Option<DateTime<Utc>>)> {
    let mut toks = line.split_whitespace();
    let time_tok = toks.next()?;
    if !time_tok.ends_with('Z') {
        return None;
    }
    // Skip the `---` separator and an optional `NEAR`.
    let lat_tok = toks.find(|t| *t != "---" && *t != "NEAR")?;
    let lon_tok = toks.next()?;
    let lat = parse_signed_coord(lat_tok, 'N', 'S')?;
    let lon = parse_signed_coord(lon_tok, 'E', 'W')?;
    Some((GeoPoint { lon, lat }, parse_jtwc_time(time_tok, issued)))
}

/// `061200Z` (DDHHMM) + issuance `(year, month, day)` -> a UTC instant.
fn parse_jtwc_time(tok: &str, issued: Option<(i32, u32, u32)>) -> Option<DateTime<Utc>> {
    let stamp = tok.trim_end_matches('Z');
    if stamp.len() != 6 || !stamp.is_ascii() {
        return None;
    }
    let day: u32 = stamp[0..2].parse().ok()?;
    let hhmm: u32 = stamp[2..6].parse().ok()?;
    resolve_forward_time(day, hhmm, issued)
}

/// `MAX SUSTAINED WINDS - 145 KT, GUSTS 175 KT` (JTWC) or
/// `MAX SUSTAINED WINDS 145 KT WITH GUSTS TO 175 KT.` (NHC) -> `145`.
fn parse_jtwc_max_wind(line: &str) -> Option<f32> {
    let rest = line.strip_prefix("MAX SUSTAINED WINDS")?;
    rest.split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())?
        .parse::<f32>()
        .ok()
}

/// The gust figure on a max-wind line: `..., GUSTS 180 KT` (JTWC) or
/// `... WITH GUSTS TO 175 KT.` (NHC) -> the first number after `GUSTS`.
fn parse_gusts(line: &str) -> Option<f32> {
    let at = line.find("GUSTS")?;
    line[at + "GUSTS".len()..]
        .split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())?
        .parse::<f32>()
        .ok()
}

/// `MOVEMENT PAST SIX HOURS - 285 DEGREES AT 11 KTS` -> `(285, 11)`; `None`
/// for the digit-less `STATIONARY` form.
fn parse_jtwc_movement(line: &str) -> Option<(f32, f32)> {
    let rest = line.trim().strip_prefix("MOVEMENT PAST SIX HOURS")?;
    let mut nums = rest
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty());
    let dir = nums.next()?.parse::<f32>().ok()?;
    let speed = nums.next()?.parse::<f32>().ok()?;
    Some((dir, speed))
}

/// The `MINIMUM CENTRAL PRESSURE AT 070000Z IS 934 MB` figure — the number
/// immediately before the `MB` token (so the DTG is never mistaken for it),
/// scanned across line wraps (live JTWC REMARKS wrap mid-sentence).
fn parse_min_pressure(text: &str) -> Option<f32> {
    let at = text.find("MINIMUM CENTRAL PRESSURE")?;
    let window: String = text[at..].chars().take(120).collect();
    let toks: Vec<&str> = window.split_whitespace().collect();
    let mb_at = toks
        .iter()
        .position(|tok| tok.trim_end_matches('.') == "MB")?;
    toks.get(mb_at.checked_sub(1)?)?.parse::<f32>().ok()
}

/// Parse the warning's identity + analysis vitals from a JTWC Tropical
/// Cyclone Warning: the `WARNING NR` sequence number, the WMO-header issue
/// DTG (`WTPN31 PGTW 070300`), the `WARNING POSITION` analysis time/position,
/// the `PRESENT WIND DISTRIBUTION` current max wind + gusts, the six-hour
/// movement, and the REMARKS minimum central pressure. This is the piece the
/// storm card shows — and the piece that replaces a lagging GDACS severity
/// (see [`sync_storm_with_geometry`]); the per-forecast-time track stays in
/// [`parse_jtwc_forecast_warning`]. `None` when the text carries none of it
/// (not a warning bulletin).
pub fn parse_jtwc_warning_info(text: &str) -> Option<WarningInfo> {
    let issued_date = jtwc_issuance_date(text);
    let lines: Vec<&str> = text.lines().collect();

    // "SUBJ/TYPHOON 09W (BAVI) WARNING NR 025//" (and its restatement).
    let number = lines.iter().find_map(|line| {
        let at = line.find("WARNING NR")?;
        line[at + "WARNING NR".len()..]
            .trim()
            .trim_end_matches('/')
            .trim()
            .parse::<u32>()
            .ok()
    });

    // WMO abbreviated heading `WTPN31 PGTW 070300`: the issue DTG (DDHHMM),
    // resolved against the REMARKS `07JUL26` issuance date.
    let issued = lines.iter().find_map(|line| {
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.len() < 3 || toks[1] != "PGTW" {
            return None;
        }
        let stamp = toks[2];
        if stamp.len() != 6 || !stamp.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        resolve_forward_time(
            stamp[..2].parse().ok()?,
            stamp[2..].parse().ok()?,
            issued_date,
        )
    });

    // The analysis section runs up to FORECASTS:/the first forecast header —
    // its position line and MAX SUSTAINED WINDS are the CURRENT state.
    let analysis_end = lines
        .iter()
        .position(|line| {
            let t = line.trim();
            t == "FORECASTS:" || t.ends_with("HRS, VALID AT:")
        })
        .unwrap_or(lines.len());
    let analysis = &lines[..analysis_end];

    // `WARNING POSITION:` then `070000Z --- NEAR 16.2N 139.9E`.
    let posit = analysis
        .iter()
        .position(|line| line.trim().starts_with("WARNING POSITION"))
        .and_then(|at| {
            analysis[at + 1..]
                .iter()
                .take(3)
                .find_map(|line| parse_jtwc_valid_line(line.trim(), issued_date))
        });
    let (position, position_time) = match posit {
        Some((point, time)) => (Some(point), time),
        None => (None, None),
    };

    // The first MAX SUSTAINED WINDS line that carries a number — skipping the
    // digit-less "MAX SUSTAINED WINDS BASED ON ONE-MINUTE AVERAGE" disclaimer
    // that precedes it in every bulletin.
    let vitals = analysis
        .iter()
        .map(|line| line.trim())
        .find(|line| parse_jtwc_max_wind(line).is_some());
    let max_wind_kt = vitals.and_then(parse_jtwc_max_wind);
    let gust_kt = vitals.and_then(parse_gusts);
    let movement = analysis.iter().find_map(|line| parse_jtwc_movement(line));
    let min_pressure_mb = parse_min_pressure(text);

    if number.is_none() && issued.is_none() && max_wind_kt.is_none() {
        return None;
    }
    Some(WarningInfo {
        agency: WarningAgency::Jtwc,
        number,
        issued,
        position_time,
        position,
        max_wind_kt,
        gust_kt,
        movement_dir_deg: movement.map(|(dir, _)| dir),
        movement_speed_kt: movement.map(|(_, speed)| speed),
        min_pressure_mb,
    })
}

// ---------------------------------------------------------------------------
// GDACS event list + geometry
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GdacsCollection {
    #[serde(default)]
    features: Vec<GdacsFeature>,
}

#[derive(Deserialize)]
struct GdacsFeature {
    #[serde(default)]
    geometry: Option<serde_json::Value>,
    properties: GdacsProps,
}

#[derive(Deserialize)]
struct GdacsProps {
    #[serde(default)]
    eventtype: String,
    #[serde(default)]
    eventid: Option<i64>,
    #[serde(default)]
    episodeid: Option<i64>,
    #[serde(default)]
    eventname: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    alertlevel: Option<String>,
    #[serde(default)]
    fromdate: Option<String>,
    #[serde(default)]
    datemodified: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    severitydata: Option<GdacsSeverity>,
    #[serde(default)]
    url: Option<GdacsUrls>,
    #[serde(default, rename = "Class")]
    class: Option<String>,
    /// On `getgeometry` forecast points: `MMDDHHMM` valid-time stamp (no year).
    #[serde(default)]
    key: Option<String>,
    /// On `getgeometry` forecast points: the analysis ("current") time —
    /// identical across a storm's points, so it is the past/forecast pivot.
    #[serde(default)]
    todate: Option<String>,
}

#[derive(Deserialize)]
struct GdacsSeverity {
    #[serde(default)]
    severity: Option<f32>,
    #[serde(default)]
    severityunit: Option<String>,
}

#[derive(Deserialize)]
struct GdacsUrls {
    #[serde(default)]
    geometry: Option<String>,
    #[serde(default)]
    report: Option<String>,
}

/// Parse GDACS `EVENTS4APP` (or `SEARCH`) output, keeping only tropical
/// cyclones.
pub fn parse_gdacs_event_list(json: &str) -> Result<Vec<TropicalCyclone>, String> {
    let parsed: GdacsCollection =
        serde_json::from_str(json).map_err(|err| format!("GDACS parse: {err}"))?;
    Ok(parsed
        .features
        .into_iter()
        .filter(|feature| feature.properties.eventtype.eq_ignore_ascii_case("TC"))
        .filter_map(gdacs_to_cyclone)
        .collect())
}

fn gdacs_to_cyclone(feature: GdacsFeature) -> Option<TropicalCyclone> {
    let position = feature.geometry.as_ref().and_then(geojson_point)?;
    let props = feature.properties;

    // GDACS severity for TC is the max sustained wind, defaulting to km/h.
    let max_wind_kt = props.severitydata.as_ref().and_then(|severity| {
        let value = severity.severity?;
        let unit = severity.severityunit.as_deref().unwrap_or("km/h");
        Some(match unit {
            u if u.eq_ignore_ascii_case("mph") => value * KT_PER_MPH,
            u if u.eq_ignore_ascii_case("kt") || u.eq_ignore_ascii_case("kn") => value,
            _ => value * KT_PER_KMH,
        })
    });
    let basin = Basin::from_lon_lat(position.lon, position.lat);
    let category = max_wind_kt.map(Category::from_wind_kt);
    let name_raw = props
        .eventname
        .or(props.name.clone())
        .unwrap_or_else(|| "Unnamed".to_owned());
    let classification = category
        .map(|category| category.label(basin))
        .unwrap_or_else(|| "Tropical Cyclone".to_owned());

    let (eventid, episodeid) = (props.eventid?, props.episodeid.unwrap_or(0));
    let urls = props.url.unwrap_or(GdacsUrls {
        geometry: None,
        report: None,
    });

    Some(TropicalCyclone {
        id: format!("gdacs:{eventid}:{episodeid}"),
        name: clean_storm_name(&name_raw),
        basin,
        source: Source::Gdacs,
        classification,
        category,
        position,
        max_wind_kt,
        gust_kt: None,
        min_pressure_mb: None,
        movement_dir_deg: None,
        movement_speed_kt: None,
        advisory_time: props
            .datemodified
            .as_deref()
            .or(props.fromdate.as_deref())
            .and_then(parse_time),
        alert_level: props.alertlevel,
        affected_areas: props.country.filter(|country| !country.is_empty()),
        forecast: Vec::new(),
        current_wind_radii: Vec::new(),
        cone: Vec::new(),
        report_url: urls.report,
        geometry_url: urls.geometry,
        // Filled later by matching the JTWC RSS feed (see
        // `attach_jtwc_forecast_urls`) when an official JTWC warning is active
        // for this storm.
        forecast_url: None,
        warning: None,
        jtwc_warning_nr: None,
    })
}

/// Parse a GDACS `getgeometry` FeatureCollection into a storm's track, cone, and
/// forecast points. Track segments are `Line_Line_<n>`; the cone is
/// `Poly_Cones`; the current center is `Point_Centroid`.
///
/// The forecast track is delivered as `Point_Polygon_Point_<n>` features — one
/// per 6/12-hourly track point (past AND future), each a small wind-radii circle
/// whose center is the track position, with a `key` (`MMDDHHMM`) valid-time
/// stamp and a `todate` analysis time. GDACS repeats only the storm's *current*
/// severity on every point (not a per-point forecast), so forecast points get
/// `max_wind_kt = None` and are colored by the storm's current category — unless
/// [`fetch_storm_geometry`] later replaces them with the JTWC warning's honest
/// per-point intensity. We keep the points strictly AFTER the analysis time (the
/// forecast; earlier points are the observed past already drawn as `Line_Line`
/// segments).
pub fn parse_gdacs_geometry(json: &str) -> Result<StormGeometry, String> {
    let parsed: GdacsCollection =
        serde_json::from_str(json).map_err(|err| format!("GDACS geometry parse: {err}"))?;

    let mut centroid = None;
    let mut cone = Vec::new();
    let mut lines: Vec<(u32, Vec<GeoPoint>)> = Vec::new();
    // (index, center, MMDDHHMM key); the year comes from `reference` below.
    let mut point_stamps: Vec<(u32, GeoPoint, String)> = Vec::new();
    let mut reference: Option<DateTime<Utc>> = None;

    for feature in &parsed.features {
        let Some(class) = feature.properties.class.as_deref() else {
            continue;
        };
        let Some(geometry) = feature.geometry.as_ref() else {
            continue;
        };
        if class == "Point_Centroid" {
            centroid = geojson_point(geometry);
        } else if class == "Poly_Cones" {
            cone = geojson_polygon_outer(geometry);
        } else if let Some(index) = class.strip_prefix("Line_Line_")
            && let Ok(index) = index.parse::<u32>()
        {
            lines.push((index, geojson_line(geometry)));
        } else if let Some(index) = class.strip_prefix("Point_Polygon_Point_")
            && let Ok(index) = index.parse::<u32>()
            && let Some(center) = geojson_polygon_centroid(geometry)
            && let Some(key) = feature.properties.key.as_deref()
        {
            if reference.is_none() {
                reference = feature.properties.todate.as_deref().and_then(parse_time);
            }
            point_stamps.push((index, center, key.to_owned()));
        }
    }

    lines.sort_by_key(|(index, _)| *index);
    // Keep each GDACS segment as its own polyline (see StormGeometry::track).
    let track = lines
        .into_iter()
        .map(|(_, points)| points)
        .filter(|points| points.len() >= 2)
        .collect();

    let forecast = gdacs_forecast_points(point_stamps, reference);

    Ok(StormGeometry {
        centroid,
        track,
        cone,
        forecast,
        // Filled from the matched JTWC warning by `fetch_storm_geometry`; GDACS
        // getgeometry alone carries no analysis-point wind radii or warning
        // identity.
        current_wind_radii: Vec::new(),
        warning: None,
    })
}

/// Resolve `MMDDHHMM` stamps against the analysis time and keep the forecast
/// (strictly-future) points, in chronological order.
fn gdacs_forecast_points(
    mut point_stamps: Vec<(u32, GeoPoint, String)>,
    reference: Option<DateTime<Utc>>,
) -> Vec<ForecastPoint> {
    let Some(reference) = reference else {
        return Vec::new();
    };
    point_stamps.sort_by_key(|(index, _, _)| *index);
    point_stamps
        .into_iter()
        .filter_map(|(_, center, key)| {
            let valid_time = gdacs_key_time(&key, reference)?;
            (valid_time > reference).then_some(ForecastPoint {
                position: center,
                valid_time: Some(valid_time),
                max_wind_kt: None,
                // GDACS getgeometry gives no per-point wind radii.
                wind_radii: Vec::new(),
            })
        })
        .collect()
}

/// `MMDDHHMM` + the analysis time -> a UTC instant. The stamp carries no year;
/// every point sits within a few days of the analysis time, so we pick the year
/// (prev/this/next) that lands the point closest to it — correct on either side
/// of a New-Year boundary.
fn gdacs_key_time(key: &str, reference: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if key.len() != 8 {
        return None;
    }
    let month: u32 = key.get(0..2)?.parse().ok()?;
    let day: u32 = key.get(2..4)?.parse().ok()?;
    let hour: u32 = key.get(4..6)?.parse().ok()?;
    let minute: u32 = key.get(6..8)?.parse().ok()?;
    let time = NaiveTime::from_hms_opt(hour, minute, 0)?;
    let base = reference.year();
    [base - 1, base, base + 1]
        .into_iter()
        .filter_map(|year| {
            let dt = NaiveDate::from_ymd_opt(year, month, day)?
                .and_time(time)
                .and_utc();
            Some(((dt - reference).num_seconds().abs(), dt))
        })
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, dt)| dt)
}

// ---- GeoJSON coordinate extraction (tolerant of lon/lat f64 nesting) -------

fn coord_pair(value: &serde_json::Value) -> Option<GeoPoint> {
    let array = value.as_array()?;
    let lon = array.first()?.as_f64()? as f32;
    let lat = array.get(1)?.as_f64()? as f32;
    Some(GeoPoint { lon, lat })
}

fn geojson_point(geometry: &serde_json::Value) -> Option<GeoPoint> {
    if geometry.get("type")?.as_str()? != "Point" {
        return None;
    }
    coord_pair(geometry.get("coordinates")?)
}

fn geojson_line(geometry: &serde_json::Value) -> Vec<GeoPoint> {
    geometry
        .get("coordinates")
        .and_then(|coords| coords.as_array())
        .map(|array| array.iter().filter_map(coord_pair).collect())
        .unwrap_or_default()
}

fn geojson_polygon_outer(geometry: &serde_json::Value) -> Vec<GeoPoint> {
    geometry
        .get("coordinates")
        .and_then(|coords| coords.as_array())
        .and_then(|rings| rings.first())
        .and_then(|ring| ring.as_array())
        .map(|array| array.iter().filter_map(coord_pair).collect())
        .unwrap_or_default()
}

/// The centroid of a polygon's outer ring (mean of its vertices). GDACS delivers
/// each forecast track point as a small wind-radii circle; its center is the
/// track position. The closing vertex duplicates the first, but on a many-vertex
/// ring that bias is far below plotting resolution.
fn geojson_polygon_centroid(geometry: &serde_json::Value) -> Option<GeoPoint> {
    let ring = geojson_polygon_outer(geometry);
    if ring.is_empty() {
        return None;
    }
    let count = ring.len() as f64;
    let (sum_lon, sum_lat) = ring.iter().fold((0.0_f64, 0.0_f64), |(lon, lat), point| {
        (lon + point.lon as f64, lat + point.lat as f64)
    });
    Some(GeoPoint {
        lon: (sum_lon / count) as f32,
        lat: (sum_lat / count) as f32,
    })
}

// ---------------------------------------------------------------------------
// Fetch + merge
// ---------------------------------------------------------------------------

pub const NHC_CURRENT_STORMS_URL: &str = "https://www.nhc.noaa.gov/CurrentStorms.json";
pub const GDACS_TC_LIST_URL: &str =
    "https://www.gdacs.org/gdacsapi/api/events/geteventlist/EVENTS4APP?eventtypes=TC";

/// Two source records whose positions sit within this great-circle distance
/// (km) describe the SAME storm. GDACS positions can lag NHC's by several
/// hours (a 15-kt storm covers ~330 km in 12 h), while two DISTINCT active
/// cyclones essentially never get this close — direct interaction (the
/// Fujiwhara regime) already begins near ~1300 km separation.
const DUPLICATE_STORM_KM: f32 = 500.0;

/// Great-circle (haversine) distance in km between two points, on the same
/// sphere the rest of this module uses ([`EARTH_RADIUS_KM`]).
fn great_circle_km(a: GeoPoint, b: GeoPoint) -> f32 {
    let (phi1, phi2) = (a.lat.to_radians(), b.lat.to_radians());
    let half_dphi = (b.lat - a.lat).to_radians() / 2.0;
    let half_dlam = (normalize_lon(b.lon - a.lon)).to_radians() / 2.0;
    let s = half_dphi.sin().powi(2) + phi1.cos() * phi2.cos() * half_dlam.sin().powi(2);
    2.0 * EARTH_RADIUS_KM * s.sqrt().clamp(-1.0, 1.0).asin()
}

/// Whether a GDACS record duplicates one of the NHC storms: a matching (real)
/// name, or a position within [`DUPLICATE_STORM_KM`]. The name check skips the
/// GDACS "Unnamed" placeholder so two sourceless invests never alias.
fn duplicates_nhc_storm(gdacs: &TropicalCyclone, nhc: &[TropicalCyclone]) -> bool {
    nhc.iter().any(|official| {
        let name_match = !gdacs.name.is_empty()
            && !gdacs.name.eq_ignore_ascii_case("unnamed")
            && gdacs.name.eq_ignore_ascii_case(&official.name);
        name_match || great_circle_km(gdacs.position, official.position) <= DUPLICATE_STORM_KM
    })
}

/// Combine NHC and GDACS into one deduplicated list. NHC is authoritative for
/// the basins it covers (Atlantic, East/Central Pacific — it carries wind AND
/// pressure), so a GDACS record of the SAME storm (matched per storm by name
/// or position proximity) is dropped in favor of NHC's. Every other GDACS
/// storm is kept — including one in an NHC basin with no NHC counterpart:
/// GDACS supplies the West Pacific/Indian Ocean/Southern Hemisphere, and it is
/// also the only witness to an Atlantic/E-Pac storm while NHC is down or has
/// not initiated advisories (the old per-BASIN filter threw those away and
/// recreated the false "quiet across every basin" that commit f090287 fixed).
/// Kept pure so the merge is unit-tested without a network.
pub fn merge_sources(
    nhc: Vec<TropicalCyclone>,
    gdacs: Vec<TropicalCyclone>,
) -> Vec<TropicalCyclone> {
    let kept_gdacs: Vec<TropicalCyclone> = gdacs
        .into_iter()
        .filter(|storm| !duplicates_nhc_storm(storm, &nhc))
        .collect();
    let mut merged = nhc;
    merged.extend(kept_gdacs);
    // Strongest first — that is the ordering the storm list wants.
    merged.sort_by(|a, b| {
        b.max_wind_kt
            .unwrap_or(0.0)
            .total_cmp(&a.max_wind_kt.unwrap_or(0.0))
    });
    merged
}

/// Fetch and merge every active tropical cyclone worldwide. Resilient: if one
/// source fails, the other's storms are still returned; only a total failure
/// is an error. As a best-effort last step, active JTWC warnings are matched to
/// the GDACS storms so each carries a `forecast_url` for per-point intensity —
/// a JTWC outage silently leaves the honest GDACS-only fallback in place.
pub fn fetch_active_cyclones(
    client: &reqwest::blocking::Client,
) -> Result<Vec<TropicalCyclone>, String> {
    let nhc =
        fetch_text(client, NHC_CURRENT_STORMS_URL).and_then(|body| parse_nhc_current_storms(&body));
    let gdacs =
        fetch_text(client, GDACS_TC_LIST_URL).and_then(|body| parse_gdacs_event_list(&body));
    let mut storms = combine_source_results(nhc, gdacs)?;
    // Best-effort: match active JTWC warnings to GDACS storms so each carries a
    // forecast_url for real per-point West-Pacific intensity (a JTWC outage
    // silently leaves the honest GDACS-only fallback in place).
    if let Ok(rss) = fetch_text(client, JTWC_RSS_URL) {
        attach_jtwc_forecast_urls(&mut storms, &parse_jtwc_rss(&rss));
    }
    Ok(storms)
}

/// Combine the two per-source fetch results into one storm list. A partial
/// failure must NOT masquerade as "no active cyclones": a single surviving
/// source is trusted only when it actually reports storms — when it is EMPTY
/// we cannot distinguish "genuinely quiet" from "the source that carried the
/// storms is down" (GDACS is the only feed for the West Pacific, NHC covers
/// just the Atlantic/E-Pac), so the failure is surfaced and the caller keeps
/// retrying instead of showing a false all-clear. When NHC is DOWN, EVERY
/// GDACS storm is kept — including those in NHC's own basins (there is no NHC
/// record to dedupe against; the old per-basin filter silently discarded a
/// GDACS-tracked Atlantic hurricane during an NHC outage, producing exactly
/// the false "Quiet across every basin" this failover exists to prevent).
/// Kept pure so every failover arm is unit-tested without a network.
pub fn combine_source_results(
    nhc: Result<Vec<TropicalCyclone>, String>,
    gdacs: Result<Vec<TropicalCyclone>, String>,
) -> Result<Vec<TropicalCyclone>, String> {
    match (nhc, gdacs) {
        (Ok(nhc), Ok(gdacs)) => Ok(merge_sources(nhc, gdacs)),
        (Ok(nhc), Err(_)) if !nhc.is_empty() => Ok(nhc),
        (Ok(_), Err(gdacs_err)) => Err(format!(
            "GDACS unavailable (NHC reports no Atlantic/E-Pac storms): {gdacs_err}"
        )),
        // NHC down, GDACS reporting: merge with an empty NHC list, which keeps
        // every GDACS storm (nothing to duplicate) and applies the wind sort.
        (Err(_), Ok(gdacs)) if !gdacs.is_empty() => Ok(merge_sources(Vec::new(), gdacs)),
        (Err(nhc_err), Ok(_)) => Err(format!(
            "NHC unavailable (GDACS reports no storms): {nhc_err}"
        )),
        (Err(nhc_err), Err(gdacs_err)) => Err(format!(
            "both sources failed — NHC: {nhc_err}; GDACS: {gdacs_err}"
        )),
    }
}

/// Fetch one storm's forecast geometry from its `geometry_url`, parsed per
/// source: GDACS `getgeometry` yields track + cone + forecast points; the NHC
/// forecast-advisory (TCM) yields forecast points (with per-point wind AND
/// 34/50/64-kt quadrant wind radii) plus the analysis-position radii.
///
/// `forecast_url` is the optional JTWC Tropical Cyclone Warning for a
/// GDACS-covered storm: when present and parseable, its per-point forecast
/// (position + valid time + **real max sustained wind**) REPLACES the
/// intensity-less GDACS forecast points, so the West-Pacific dots color by the
/// official JTWC per-point Saffir–Simpson category. The GDACS track and cone are
/// always kept. A failed/empty JTWC fetch leaves the GDACS fallback untouched.
pub fn fetch_storm_geometry(
    client: &reqwest::blocking::Client,
    source: Source,
    url: &str,
    forecast_url: Option<&str>,
) -> Result<StormGeometry, String> {
    let body = fetch_text(client, url)?;
    match source {
        Source::Gdacs => {
            let mut geometry = parse_gdacs_geometry(&body)?;
            if let Some(warning_url) = forecast_url
                && let Ok(warning) = fetch_text(client, warning_url)
            {
                apply_jtwc_warning(&mut geometry, &warning);
            }
            Ok(geometry)
        }
        Source::Nhc => Ok(nhc_geometry_from_forecast_advisory(&body)),
    }
}

/// Apply a fetched JTWC Tropical Cyclone Warning bulletin to a GDACS storm's
/// geometry — the pure core of [`fetch_storm_geometry`]'s enrichment, split
/// out so a warning-N → warning-N+1 replacement is provable without a
/// network. The warning's per-point forecast REPLACES the intensity-less
/// GDACS points, the analysis-point 34/50/64-kt radii anchor the wind-rose /
/// danger-area rendering at the storm's current position, and the warning
/// identity + analysis vitals ride along for the card and the GDACS-severity
/// override. An unparseable bulletin leaves the GDACS fallback untouched.
pub fn apply_jtwc_warning(geometry: &mut StormGeometry, warning_text: &str) {
    let jtwc = parse_jtwc_forecast_warning(warning_text);
    if !jtwc.is_empty() {
        geometry.forecast = jtwc;
        geometry.current_wind_radii = parse_jtwc_current_radii(warning_text);
    }
    if let Some(info) = parse_jtwc_warning_info(warning_text) {
        geometry.warning = Some(info);
    }
}

/// Mirror a fetched [`StormGeometry`] onto its storm record — the re-attach
/// the UI runs every frame (a fresh storms list arrives with empty
/// forecast/radii/warning, and the geometry map is the transport).
///
/// For a GDACS-sourced storm carrying a JTWC warning, the warning's analysis
/// vitals REPLACE the aggregator's: GDACS repeats a storm's last-processed
/// severity for many hours after JTWC has issued newer warnings (the "Bavi
/// still shows 155 kt two warnings after JTWC dropped it to 125 kt" bug), so
/// the official analysis wind/gusts/pressure/position/motion win and the
/// category + classification are recomputed from the official wind. NHC
/// storms keep their `CurrentStorms.json` vitals (refreshed with every public
/// advisory, at least as fresh as the TCM) — only the advisory identity is
/// attached for display.
pub fn sync_storm_with_geometry(storm: &mut TropicalCyclone, geometry: &StormGeometry) {
    if storm.forecast != geometry.forecast {
        storm.forecast = geometry.forecast.clone();
    }
    if storm.current_wind_radii != geometry.current_wind_radii {
        storm.current_wind_radii = geometry.current_wind_radii.clone();
    }
    if storm.warning != geometry.warning {
        storm.warning = geometry.warning.clone();
    }
    let Some(warning) = &storm.warning else {
        return;
    };
    if warning.agency != WarningAgency::Jtwc || storm.source != Source::Gdacs {
        return;
    }
    if let Some(kt) = warning.max_wind_kt {
        storm.max_wind_kt = Some(kt);
        let category = Category::from_wind_kt(kt);
        storm.category = Some(category);
        storm.classification = category.label(storm.basin);
    }
    if warning.gust_kt.is_some() {
        storm.gust_kt = warning.gust_kt;
    }
    if warning.min_pressure_mb.is_some() {
        storm.min_pressure_mb = warning.min_pressure_mb;
    }
    if warning.movement_dir_deg.is_some() && warning.movement_speed_kt.is_some() {
        storm.movement_dir_deg = warning.movement_dir_deg;
        storm.movement_speed_kt = warning.movement_speed_kt;
    }
    // The official analysis position (the wind radii are valid ABOUT it, so
    // the rose/danger area anchor correctly even when GDACS's centroid lags).
    if let Some(position) = warning.position {
        storm.position = position;
    }
}

/// Build an NHC storm's geometry from its Forecast/Advisory (TCM) text: the
/// per-point forecast track (position, valid time, max wind, quadrant wind
/// radii) plus the analysis-block 34/50/64-kt radii at the current position.
/// Cone and past track stay empty (no GIS product is fetched); the wind-radii
/// rose + 34-kt danger-area renderer gives NHC storms their footprint on the
/// map, exactly as it does for JTWC-matched West-Pacific storms.
pub fn nhc_geometry_from_forecast_advisory(text: &str) -> StormGeometry {
    StormGeometry {
        forecast: parse_nhc_forecast_advisory(text),
        current_wind_radii: parse_nhc_current_radii(text),
        warning: parse_nhc_warning_info(text),
        ..StormGeometry::default()
    }
}

fn fetch_text(client: &reqwest::blocking::Client, url: &str) -> Result<String, String> {
    let response = client
        .get(url)
        .send()
        .map_err(|err| format!("GET {url}: {err}"))?;
    if !response.status().is_success() {
        return Err(format!("GET {url}: HTTP {}", response.status()));
    }
    response.text().map_err(|err| format!("body {url}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NHC: &str = include_str!("../tests/fixtures/tropical/nhc_active_storms.json");
    const GDACS_LIST: &str = include_str!("../tests/fixtures/tropical/gdacs_tc_list.json");
    const GDACS_GEOM: &str = include_str!("../tests/fixtures/tropical/gdacs_bavi_geometry.json");
    // Real products captured live for the forecast-dot feature:
    //  - a trimmed BAVI-26 `getgeometry` carrying `Point_Polygon_Point_*`
    //    forecast points (real centers/keys/`todate`, minimal rings), and
    //  - Hurricane Milton's actual Forecast/Advisory (TCM) #15 (AL142024).
    const GDACS_FCST: &str =
        include_str!("../tests/fixtures/tropical/gdacs_bavi_forecast_geometry.json");
    // BAVI-26's live `getgeometry` with the (large) impact polygons trimmed —
    // its real 218-vertex cone spans ~27° of longitude (Guam → Philippine Sea),
    // the wide, partly-off-screen cone the app_ui overlay must still draw.
    const GDACS_WIDE_CONE: &str =
        include_str!("../tests/fixtures/tropical/gdacs_bavi_wide_cone_geometry.json");
    const NHC_TCM: &str =
        include_str!("../tests/fixtures/tropical/nhc_milton_forecast_advisory.txt");
    // Real JTWC products captured live for the West-Pacific per-point intensity
    // feature: the active RSS feed and Super Typhoon 09W (BAVI) Warning #21.
    const JTWC_RSS: &str = include_str!("../tests/fixtures/tropical/jtwc_rss.xml");
    const JTWC_WARNING: &str = include_str!("../tests/fixtures/tropical/jtwc_bavi_warning.txt");
    // The SAME storm one day later (captured live 2026-07-07): Warning #25,
    // downgraded to 125 kt — the newer-warning-replaces-older proof, and the
    // wrapped-REMARKS pressure form.
    const JTWC_WARNING_25: &str =
        include_str!("../tests/fixtures/tropical/jtwc_bavi_warning_25.txt");

    #[test]
    fn nhc_parses_storm_vitals() {
        let storms = parse_nhc_current_storms(NHC).expect("parse");
        assert_eq!(storms.len(), 1);
        let s = &storms[0];
        assert_eq!(s.name, "Alberto");
        assert_eq!(s.id, "nhc:al012026");
        assert_eq!(s.basin, Basin::Atlantic);
        assert_eq!(s.source, Source::Nhc);
        assert_eq!(s.max_wind_kt, Some(85.0));
        assert_eq!(s.min_pressure_mb, Some(968.0));
        assert_eq!(s.movement_dir_deg, Some(340.0));
        assert_eq!(s.category, Some(Category::Two)); // 85 kt
        assert_eq!(s.classification, "Category 2 Hurricane");
        assert!(s.advisory_time.is_some());
        assert!(s.report_url.as_deref().unwrap().contains("nhc.noaa.gov"));
        assert!((s.position.lat - 24.5).abs() < 1e-3);
        assert!((s.position.lon + 88.9).abs() < 1e-3);
    }

    #[test]
    fn gdacs_parses_live_typhoon() {
        let storms = parse_gdacs_event_list(GDACS_LIST).expect("parse");
        assert_eq!(storms.len(), 2, "BAVI + MAYSAK");
        let bavi = storms
            .iter()
            .find(|s| s.name == "Bavi")
            .expect("BAVI present");
        assert_eq!(bavi.basin, Basin::WestPacific);
        assert_eq!(bavi.source, Source::Gdacs);
        assert_eq!(bavi.alert_level.as_deref(), Some("Red"));
        // 268.5 km/h -> ~145 kt -> Cat 5 -> Super Typhoon in the W Pacific.
        let wind = bavi.max_wind_kt.expect("wind");
        assert!((wind - 145.0).abs() < 2.0, "wind={wind}");
        assert_eq!(bavi.category, Some(Category::Five));
        assert_eq!(bavi.classification, "Super Typhoon");
        assert!((bavi.position.lon - 148.9).abs() < 1e-3);
        assert!((bavi.position.lat - 12.9).abs() < 1e-3);
        assert!(
            bavi.geometry_url
                .as_deref()
                .unwrap()
                .contains("getgeometry")
        );
        assert!(bavi.affected_areas.as_deref().unwrap().contains("Guam"));
    }

    #[test]
    fn gdacs_geometry_extracts_track_centroid_cone() {
        let geometry = parse_gdacs_geometry(GDACS_GEOM).expect("parse");
        let centroid = geometry.centroid.expect("centroid");
        assert!((centroid.lon - 148.9).abs() < 1.0);
        assert!(!geometry.track.is_empty(), "track points present");
        assert!(geometry.cone.len() >= 3, "cone is a polygon ring");
    }

    #[test]
    fn nhc_tcm_parses_forecast_points_with_intensity() {
        // Milton (AL142024) advisory 15, issued 2100 UTC TUE OCT 08 2024.
        let fc = parse_nhc_forecast_advisory(NHC_TCM);
        // 6 `FORECAST VALID` (to day 3) + 2 `OUTLOOK VALID` (days 4–5).
        assert_eq!(fc.len(), 8, "forecast + outlook points");

        // First point: FORECAST VALID 09/0600Z 23.8N 86.4W / MAX WIND 145 KT.
        let first = &fc[0];
        assert!((first.position.lat - 23.8).abs() < 1e-3);
        assert!(
            (first.position.lon + 86.4).abs() < 1e-3,
            "W longitude is negative"
        );
        assert_eq!(first.max_wind_kt, Some(145.0));
        assert_eq!(
            first.max_wind_kt.map(Category::from_wind_kt),
            Some(Category::Five)
        );
        let expect = NaiveDate::from_ymd_opt(2024, 10, 9)
            .unwrap()
            .and_hms_opt(6, 0, 0)
            .unwrap()
            .and_utc();
        assert_eq!(first.valid_time, Some(expect));

        // Per-point intensity really varies (the whole point of the feature):
        // 145 → 130 → 110 → 75 kt, i.e. Cat 5 → 4 → 3 → 1.
        assert_eq!(
            fc[1].max_wind_kt.map(Category::from_wind_kt),
            Some(Category::Four)
        );
        assert_eq!(
            fc[2].max_wind_kt.map(Category::from_wind_kt),
            Some(Category::Three)
        );
        assert_eq!(
            fc[3].max_wind_kt.map(Category::from_wind_kt),
            Some(Category::One)
        );
        assert_eq!(
            fc[5].max_wind_kt.map(Category::from_wind_kt),
            Some(Category::TropicalStorm)
        );

        // Valid times are strictly increasing.
        for pair in fc.windows(2) {
            assert!(pair[0].valid_time.unwrap() < pair[1].valid_time.unwrap());
        }
    }

    #[test]
    fn nhc_tcm_time_rolls_over_month_and_year() {
        // Synthetic edge check only: a late-December advisory whose forecast
        // days wrap into January of the next year (no live storm exercises it).
        let text = "\
0300 UTC WED DEC 31 2025
FORECAST VALID 31/1200Z 25.0N 70.0W
MAX WIND 60 KT...GUSTS 75 KT.
FORECAST VALID 02/0000Z 28.0N 68.0W
MAX WIND 50 KT...GUSTS 65 KT.
";
        let fc = parse_nhc_forecast_advisory(text);
        assert_eq!(fc.len(), 2);
        assert_eq!(
            fc[0].valid_time,
            NaiveDate::from_ymd_opt(2025, 12, 31)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .map(|dt| dt.and_utc())
        );
        assert_eq!(
            fc[1].valid_time,
            NaiveDate::from_ymd_opt(2026, 1, 2)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .map(|dt| dt.and_utc())
        );
    }

    #[test]
    fn jtwc_rss_lists_active_warning_and_rejects_outlooks() {
        let refs = parse_jtwc_rss(JTWC_RSS);
        // Exactly one active storm warning; the basin-wide ABPW/ABIO outlook
        // `web.txt` products (no storm number) must NOT be treated as warnings.
        assert_eq!(refs.len(), 1, "one active storm warning: {refs:?}");
        let r = &refs[0];
        assert_eq!(r.designation, "09W");
        assert_eq!(r.name, "Bavi");
        assert_eq!(
            r.warning_url,
            "https://www.metoc.navy.mil/jtwc/products/wp0926web.txt"
        );
        // The advertised sequence number — the geometry cache's refetch signal.
        assert_eq!(r.warning_nr, Some(21));
        // The filename gate distinguishes storm warnings from basin outlooks.
        assert!(is_jtwc_warning_url(
            "https://www.metoc.navy.mil/jtwc/products/wp0926web.txt"
        ));
        assert!(!is_jtwc_warning_url(
            "https://www.metoc.navy.mil/jtwc/products/abpwweb.txt"
        ));
        assert!(!is_jtwc_warning_url(
            "https://www.metoc.navy.mil/jtwc/products/abioweb.txt"
        ));
    }

    #[test]
    fn jtwc_warning_parses_forecast_points_with_intensity() {
        let fc = parse_jtwc_forecast_warning(JTWC_WARNING);
        // 3 FORECASTS + 3 EXTENDED OUTLOOK + 2 LONG RANGE = 8 forecast points;
        // the current WARNING POSITION (analysis) point is excluded.
        assert_eq!(fc.len(), 8, "forecast + outlook points");

        // First point: 061200Z --- 15.1N 142.5E / MAX SUSTAINED WINDS 145 KT.
        let first = &fc[0];
        assert!((first.position.lat - 15.1).abs() < 1e-3);
        assert!(
            (first.position.lon - 142.5).abs() < 1e-3,
            "West-Pacific longitude is positive (E)"
        );
        assert_eq!(first.max_wind_kt, Some(145.0));
        assert_eq!(
            first.max_wind_kt.map(Category::from_wind_kt),
            Some(Category::Five)
        );
        // Issuance 06JUL26 → first valid time 2026-07-06 12:00Z.
        assert_eq!(
            first.valid_time,
            NaiveDate::from_ymd_opt(2026, 7, 6)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .map(|dt| dt.and_utc())
        );

        // Real per-point intensity: 145,145,150,145,140,135,125,110 kt, i.e.
        // Cat 5 holding then weakening through Cat 4 to Cat 3 by 120 h.
        let cats: Vec<_> = fc
            .iter()
            .map(|p| p.max_wind_kt.map(Category::from_wind_kt))
            .collect();
        assert_eq!(
            cats,
            vec![
                Some(Category::Five),
                Some(Category::Five),
                Some(Category::Five),
                Some(Category::Five),
                Some(Category::Five),
                Some(Category::Four),
                Some(Category::Four),
                Some(Category::Three),
            ]
        );
        // Every forecast point carries an honest per-point wind (the whole point).
        assert!(fc.iter().all(|p| p.max_wind_kt.is_some()));

        // Last point: 110000Z --- 25.9N 122.4E, five days out.
        let last = fc.last().unwrap();
        assert!((last.position.lat - 25.9).abs() < 1e-3);
        assert!((last.position.lon - 122.4).abs() < 1e-3);
        assert_eq!(last.max_wind_kt, Some(110.0));

        // Valid times strictly increasing.
        for pair in fc.windows(2) {
            assert!(pair[0].valid_time.unwrap() < pair[1].valid_time.unwrap());
        }
    }

    #[test]
    fn jtwc_forecast_url_attaches_to_matching_gdacs_storm() {
        // GDACS list carries BAVI + MAYSAK; RSS has an active warning for BAVI.
        let mut storms = parse_gdacs_event_list(GDACS_LIST).unwrap();
        let refs = parse_jtwc_rss(JTWC_RSS);
        attach_jtwc_forecast_urls(&mut storms, &refs);
        let bavi = storms.iter().find(|s| s.name == "Bavi").unwrap();
        assert_eq!(
            bavi.forecast_url.as_deref(),
            Some("https://www.metoc.navy.mil/jtwc/products/wp0926web.txt"),
            "BAVI matched to its JTWC warning by name"
        );
        assert_eq!(
            bavi.jtwc_warning_nr,
            Some(21),
            "RSS warning number rides along"
        );
        // MAYSAK has no active JTWC warning in the feed → no forecast URL.
        let maysak = storms.iter().find(|s| s.name == "Maysak").unwrap();
        assert_eq!(maysak.forecast_url, None);
        assert_eq!(maysak.jtwc_warning_nr, None);
    }

    #[test]
    fn feedback_v03412_jtwc_name_match_never_overrides_an_nhc_basin() {
        let warning = JtwcWarningRef {
            designation: "09W".to_owned(),
            name: "Bavi".to_owned(),
            warning_url: "https://www.metoc.navy.mil/jtwc/products/wp0926web.txt".to_owned(),
            warning_nr: Some(25),
        };
        let mut storms = vec![
            synthetic_storm("gdacs:atl", "Bavi", Source::Gdacs, -60.0, 25.0, 70.0),
            synthetic_storm("gdacs:wp", "Bavi", Source::Gdacs, 130.0, 20.0, 70.0),
        ];

        attach_jtwc_forecast_urls(&mut storms, &[warning]);

        assert_eq!(storms[0].basin, Basin::Atlantic);
        assert_eq!(storms[0].forecast_url, None);
        assert_eq!(storms[0].jtwc_warning_nr, None);
        assert_eq!(storms[1].basin, Basin::WestPacific);
        assert!(storms[1].forecast_url.is_some());
        assert_eq!(storms[1].jtwc_warning_nr, Some(25));
    }

    #[test]
    fn jtwc_intensity_replaces_intensityless_gdacs_forecast() {
        // Mirrors the `fetch_storm_geometry` enrichment: the GDACS getgeometry
        // forecast points carry NO honest wind; the matched JTWC warning
        // replaces them with real per-point intensity while GDACS keeps the
        // track/cone. This is what turns the West-Pacific dots from "current
        // category on every dot" into official per-point Saffir–Simpson colors.
        let mut geometry = parse_gdacs_geometry(GDACS_FCST).unwrap();
        assert!(
            geometry.forecast.iter().all(|p| p.max_wind_kt.is_none()),
            "GDACS alone gives no per-point wind"
        );
        let jtwc = parse_jtwc_forecast_warning(JTWC_WARNING);
        assert!(!jtwc.is_empty());
        geometry.forecast = jtwc;
        // The track/cone survive from GDACS; the forecast now colors per point.
        assert!(!geometry.track.is_empty() && geometry.cone.len() >= 3);
        assert!(geometry.forecast.iter().all(|p| p.max_wind_kt.is_some()));
        let categories: Vec<_> = geometry
            .forecast
            .iter()
            .filter_map(|p| p.max_wind_kt.map(Category::from_wind_kt))
            .collect();
        assert!(
            categories.iter().any(|c| *c != categories[0]),
            "per-point intensity spans multiple categories: {categories:?}"
        );
    }

    #[test]
    fn jtwc_warning_time_rolls_over_month() {
        // Synthetic edge check only: a warning issued 30 SEP whose long-range
        // forecast days wrap into October (no live storm exercises rollover).
        let text = "\
SUBJ/TYPHOON 20W (TEST) WARNING NR 001//
   FORECASTS:
   12 HRS, VALID AT:
   301200Z --- 20.0N 130.0E
   MAX SUSTAINED WINDS - 80 KT, GUSTS 100 KT
   120 HRS, VALID AT:
   050000Z --- 28.0N 128.0E
   MAX SUSTAINED WINDS - 45 KT, GUSTS 60 KT
REMARKS:
30SEP26. TYPHOON 20W (TEST).//
";
        let fc = parse_jtwc_forecast_warning(text);
        assert_eq!(fc.len(), 2);
        assert_eq!(
            fc[0].valid_time,
            NaiveDate::from_ymd_opt(2026, 9, 30)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .map(|dt| dt.and_utc())
        );
        assert_eq!(
            fc[1].valid_time,
            NaiveDate::from_ymd_opt(2026, 10, 5)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .map(|dt| dt.and_utc())
        );
    }

    #[test]
    fn gdacs_geometry_extracts_forecast_points() {
        let geom = parse_gdacs_geometry(GDACS_FCST).expect("parse");
        // Still yields the observed pieces.
        let centroid = geom.centroid.expect("centroid");
        assert!((centroid.lon - 145.0).abs() < 0.5 && (centroid.lat - 14.3).abs() < 0.5);
        assert!(!geom.track.is_empty());
        assert!(geom.cone.len() >= 3);

        // Analysis time is 2026-07-06T00:00Z; only strictly-later points are
        // forecast, so past points 18/19 and the current point 20 drop out and
        // 21/22/28 remain, in chronological order.
        assert_eq!(geom.forecast.len(), 3, "future-only");
        let f0 = &geom.forecast[0];
        assert!((f0.position.lon - 142.5).abs() < 1e-2);
        assert!((f0.position.lat - 15.1).abs() < 1e-2);
        assert_eq!(
            f0.valid_time,
            NaiveDate::from_ymd_opt(2026, 7, 6)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .map(|dt| dt.and_utc())
        );
        // GDACS repeats only the current severity, so there is no honest
        // per-point forecast wind — left None (dot inherits current category).
        assert!(geom.forecast.iter().all(|p| p.max_wind_kt.is_none()));

        let last = geom.forecast.last().unwrap();
        assert!((last.position.lon - 122.4).abs() < 1e-2);
        assert_eq!(
            last.valid_time,
            NaiveDate::from_ymd_opt(2026, 7, 11)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .map(|dt| dt.and_utc())
        );

        let reference = NaiveDate::from_ymd_opt(2026, 7, 6)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        assert!(
            geom.forecast
                .iter()
                .all(|p| p.valid_time.unwrap() > reference)
        );
    }

    #[test]
    fn gdacs_geometry_extracts_wide_cone() {
        // The real BAVI cone is a large ring; the app_ui overlay relies on it
        // being delivered intact (it derives the on-screen jump limit from the
        // cone's own geographic span).
        let geom = parse_gdacs_geometry(GDACS_WIDE_CONE).expect("parse");
        assert!(
            geom.cone.len() >= 200,
            "real wide cone ring: {} vertices",
            geom.cone.len()
        );
        let west = geom.cone.iter().fold(f32::INFINITY, |m, p| m.min(p.lon));
        let east = geom
            .cone
            .iter()
            .fold(f32::NEG_INFINITY, |m, p| m.max(p.lon));
        assert!(east - west > 20.0, "cone spans a wide longitude range");
        assert!(!geom.track.is_empty(), "track segments present");
    }

    #[test]
    fn empty_nhc_is_no_storms() {
        assert!(
            parse_nhc_current_storms(r#"{"activeStorms":[]}"#)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn merge_keeps_both_sources_and_sorts_by_wind() {
        let nhc = parse_nhc_current_storms(NHC).unwrap(); // Atlantic "Alberto"
        let gdacs = parse_gdacs_event_list(GDACS_LIST).unwrap(); // W Pacific BAVI + MAYSAK
        let merged = merge_sources(nhc, gdacs);
        // NHC Atlantic storm kept; both W-Pacific GDACS storms kept (no dupes).
        assert_eq!(merged.len(), 3);
        // Strongest first: BAVI (~145 kt) leads.
        assert_eq!(merged[0].name, "Bavi");
        assert!(merged[0].max_wind_kt.unwrap() >= merged[1].max_wind_kt.unwrap());
    }

    /// Minimal hand-built record for the merge/failover tests (the committed
    /// fixtures carry no GDACS storm inside an NHC basin).
    fn synthetic_storm(
        id: &str,
        name: &str,
        source: Source,
        lon: f32,
        lat: f32,
        wind_kt: f32,
    ) -> TropicalCyclone {
        TropicalCyclone {
            id: id.to_owned(),
            name: name.to_owned(),
            basin: Basin::from_lon_lat(lon, lat),
            source,
            classification: "Tropical Cyclone".to_owned(),
            category: Some(Category::from_wind_kt(wind_kt)),
            position: GeoPoint { lon, lat },
            max_wind_kt: Some(wind_kt),
            gust_kt: None,
            min_pressure_mb: None,
            movement_dir_deg: None,
            movement_speed_kt: None,
            advisory_time: None,
            alert_level: None,
            affected_areas: None,
            forecast: Vec::new(),
            current_wind_radii: Vec::new(),
            cone: Vec::new(),
            report_url: None,
            geometry_url: None,
            forecast_url: None,
            warning: None,
            jtwc_warning_nr: None,
        }
    }

    #[test]
    fn merge_dedupes_per_storm_not_per_basin() {
        // Audit #1: a legitimate GDACS-tracked Atlantic system with NO NHC
        // counterpart (NHC hasn't initiated advisories) must survive the
        // merge — the old per-basin filter silently discarded it.
        let nhc = parse_nhc_current_storms(NHC).unwrap(); // Alberto, 24.5N 88.9W
        let oscar = synthetic_storm("gdacs:900001:3", "Oscar", Source::Gdacs, -60.0, 30.0, 90.0);
        let mut gdacs = parse_gdacs_event_list(GDACS_LIST).unwrap();
        gdacs.push(oscar);
        let merged = merge_sources(nhc, gdacs);
        assert_eq!(merged.len(), 4, "Alberto + BAVI + MAYSAK + Oscar");
        assert!(
            merged.iter().any(|s| s.name == "Oscar"
                && s.source == Source::Gdacs
                && s.basin == Basin::Atlantic),
            "non-duplicate GDACS storm in an NHC basin survives: {merged:?}"
        );
    }

    #[test]
    fn merge_drops_gdacs_duplicate_by_name() {
        // GDACS's record of the SAME hurricane, position drifted well past the
        // proximity gate — the (case-insensitive) name still identifies it.
        let nhc = parse_nhc_current_storms(NHC).unwrap();
        let dup = synthetic_storm(
            "gdacs:900002:1",
            "ALBERTO",
            Source::Gdacs,
            -70.0,
            35.0,
            80.0,
        );
        let merged = merge_sources(nhc, vec![dup]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].source, Source::Nhc, "the NHC record wins");
    }

    #[test]
    fn merge_drops_gdacs_duplicate_by_position() {
        // Same storm under a placeholder name: caught by position proximity.
        // An unrelated far-away placeholder must NOT alias (names don't match
        // and "Unnamed" is excluded from name matching anyway).
        let nhc = parse_nhc_current_storms(NHC).unwrap(); // Alberto, 24.5N 88.9W
        let near_dup = synthetic_storm(
            "gdacs:900003:1",
            "Unnamed",
            Source::Gdacs,
            -88.5,
            24.8,
            80.0,
        );
        let far_invest = synthetic_storm(
            "gdacs:900004:1",
            "Unnamed",
            Source::Gdacs,
            -30.0,
            20.0,
            45.0,
        );
        let merged = merge_sources(nhc, vec![near_dup, far_invest]);
        assert_eq!(merged.len(), 2, "{merged:?}");
        assert!(merged.iter().any(|s| s.id == "gdacs:900004:1"));
        assert!(!merged.iter().any(|s| s.id == "gdacs:900003:1"));
    }

    #[test]
    fn combine_keeps_all_gdacs_storms_when_nhc_is_down() {
        // Audit #1 regression: during an NHC outage, GDACS's record of an
        // Atlantic hurricane is the only witness. The old code ran the
        // per-basin filter anyway and returned Ok(vec![]) — a false
        // "Quiet across every basin" that also reset the aggressive retry.
        let atlantic = synthetic_storm(
            "gdacs:900005:2",
            "Milton",
            Source::Gdacs,
            -87.5,
            25.0,
            120.0,
        );
        let wpac = synthetic_storm("gdacs:900006:4", "Bavi", Source::Gdacs, 145.0, 14.3, 145.0);
        let out = combine_source_results(Err("NHC HTTP 503".to_owned()), Ok(vec![atlantic, wpac]))
            .expect("a reporting GDACS alone is trusted");
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(
            out.iter()
                .any(|s| s.name == "Milton" && s.basin == Basin::Atlantic),
            "the NHC-basin storm survives the NHC outage: {out:?}"
        );
        // Strongest first still applies on the failover path.
        assert_eq!(out[0].name, "Bavi");
    }

    #[test]
    fn combine_failover_arms_never_fake_an_all_clear() {
        let nhc = || parse_nhc_current_storms(NHC).unwrap(); // 1 storm
        let gdacs = || parse_gdacs_event_list(GDACS_LIST).unwrap(); // 2 storms
        // Both ok -> merged.
        assert_eq!(
            combine_source_results(Ok(nhc()), Ok(gdacs()))
                .unwrap()
                .len(),
            3
        );
        // NHC ok + reporting, GDACS down -> NHC's storms survive.
        let out = combine_source_results(Ok(nhc()), Err("GDACS down".to_owned())).unwrap();
        assert_eq!(out.len(), 1);
        // A surviving-but-EMPTY source cannot prove "quiet" -> error (retry).
        assert!(combine_source_results(Ok(Vec::new()), Err("GDACS down".to_owned())).is_err());
        assert!(combine_source_results(Err("NHC down".to_owned()), Ok(Vec::new())).is_err());
        // Both down -> error carrying both causes.
        let err = combine_source_results(Err("NHC down".to_owned()), Err("GDACS down".to_owned()))
            .unwrap_err();
        assert!(
            err.contains("NHC down") && err.contains("GDACS down"),
            "{err}"
        );
    }

    #[test]
    fn empty_nhc_feed_does_not_hide_gdacs_atlantic_storm() {
        // NHC responding but with no active systems listed yet; GDACS already
        // carries the storm. The merge keeps it (nothing to duplicate).
        let atlantic =
            synthetic_storm("gdacs:900007:1", "Nadine", Source::Gdacs, -80.0, 27.0, 70.0);
        let merged =
            combine_source_results(Ok(Vec::new()), Ok(vec![atlantic])).expect("both sources ok");
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "Nadine");
    }

    #[test]
    fn display_helpers_format_vitals() {
        let bavi = parse_gdacs_event_list(GDACS_LIST)
            .unwrap()
            .into_iter()
            .find(|s| s.name == "Bavi")
            .unwrap();
        let wind = bavi.wind_summary().expect("wind");
        assert!(
            wind.contains("kt") && wind.contains("mph") && wind.contains("km/h"),
            "{wind}"
        );
        assert!(wind.starts_with("145 kt"), "{wind}");

        let alberto = parse_nhc_current_storms(NHC).unwrap().pop().unwrap();
        assert_eq!(alberto.pressure_summary().as_deref(), Some("968 mb"));
        assert_eq!(
            alberto.motion_summary().as_deref(),
            Some("NNW (340°) at 12 kt")
        );
    }

    #[test]
    fn compass_16_bins() {
        assert_eq!(compass_16(0.0), "N");
        assert_eq!(compass_16(45.0), "NE");
        assert_eq!(compass_16(315.0), "NW");
        assert_eq!(compass_16(340.0), "NNW");
        assert_eq!(compass_16(359.0), "N");
    }

    #[test]
    fn saffir_simpson_bins_and_basin_nouns() {
        assert_eq!(Category::from_wind_kt(30.0), Category::TropicalDepression);
        assert_eq!(Category::from_wind_kt(50.0), Category::TropicalStorm);
        assert_eq!(Category::from_wind_kt(140.0), Category::Five);
        assert_eq!(
            Category::Four.label(Basin::Atlantic),
            "Category 4 Hurricane"
        );
        assert_eq!(Category::Five.label(Basin::WestPacific), "Super Typhoon");
        assert_eq!(
            Category::Three.label(Basin::WestPacific),
            "Category 3 Typhoon"
        );
    }

    #[test]
    fn jtwc_warning_parses_per_point_wind_radii() {
        let fc = parse_jtwc_forecast_warning(JTWC_WARNING);
        assert_eq!(fc.len(), 8);

        // First forecast point (12 HRS, 061200Z --- 15.1N 142.5E) carries all
        // three thresholds, in the bulletin's strongest-first order (64,50,34).
        let first = &fc[0];
        assert_eq!(
            first.wind_radii.iter().map(|r| r.kt).collect::<Vec<_>>(),
            vec![64, 50, 34]
        );
        let r64 = first.wind_radii.iter().find(|r| r.kt == 64).unwrap();
        assert_eq!(
            (r64.ne_nm, r64.se_nm, r64.sw_nm, r64.nw_nm),
            (60.0, 60.0, 40.0, 60.0)
        );
        let r50 = first.wind_radii.iter().find(|r| r.kt == 50).unwrap();
        assert_eq!(
            (r50.ne_nm, r50.se_nm, r50.sw_nm, r50.nw_nm),
            (110.0, 90.0, 90.0, 110.0)
        );
        let r34 = first.wind_radii.iter().find(|r| r.kt == 34).unwrap();
        assert_eq!(
            (r34.ne_nm, r34.se_nm, r34.sw_nm, r34.nw_nm),
            (260.0, 230.0, 190.0, 230.0),
            "34-kt gale radii tightest/asymmetric at the strong early point"
        );

        // Last point (120 HRS, 110000Z --- 25.9N 122.4E): the 34-kt field has
        // fanned wide on the NE/SE side and shrunk on the SW/NW side.
        let last = fc.last().unwrap();
        let l34 = last.wind_radii.iter().find(|r| r.kt == 34).unwrap();
        assert_eq!(
            (l34.ne_nm, l34.se_nm, l34.sw_nm, l34.nw_nm),
            (290.0, 270.0, 190.0, 130.0)
        );

        // The block-slicing must NOT bleed one point's radii into another: the
        // 64-kt SW radius really does step 40 → 50 → 60 across the first three
        // forecast times (proves per-block scoping).
        let sw64: Vec<f32> = fc
            .iter()
            .take(3)
            .map(|p| p.wind_radii.iter().find(|r| r.kt == 64).unwrap().sw_nm)
            .collect();
        assert_eq!(sw64, vec![40.0, 50.0, 60.0]);
    }

    #[test]
    fn jtwc_current_radii_parses_analysis_block() {
        // PRESENT WIND DISTRIBUTION at WARNING POSITION 060000Z --- 14.3N 145.0E.
        let radii = parse_jtwc_current_radii(JTWC_WARNING);
        // Exactly the three thresholds — the "POSITION ACCURATE TO WITHIN 020
        // NM" line (which precedes the PRESENT WIND DISTRIBUTION block, before
        // any RADIUS OF header) is NOT mistaken for a fourth wind radius.
        assert_eq!(
            radii.iter().map(|r| r.kt).collect::<Vec<_>>(),
            vec![64, 50, 34]
        );
        let r64 = radii.iter().find(|r| r.kt == 64).unwrap();
        assert_eq!(
            (r64.ne_nm, r64.se_nm, r64.sw_nm, r64.nw_nm),
            (60.0, 50.0, 50.0, 60.0)
        );
        let r34 = radii.iter().find(|r| r.kt == 34).unwrap();
        assert_eq!(
            (r34.ne_nm, r34.se_nm, r34.sw_nm, r34.nw_nm),
            (270.0, 245.0, 200.0, 230.0)
        );
    }

    #[test]
    fn wind_radii_single_radius_form_is_symmetric() {
        // Synthetic edge check: the ATCF single-radius form (no quadrant word)
        // means all four quadrants equal — no live BAVI point exercises it.
        let lines = ["RADIUS OF 034 KT WINDS - 100 NM"];
        let radii = parse_wind_radii_lines(&lines);
        assert_eq!(radii.len(), 1);
        let r = radii[0];
        assert_eq!(
            (r.kt, r.ne_nm, r.se_nm, r.sw_nm, r.nw_nm),
            (34, 100.0, 100.0, 100.0, 100.0)
        );
    }

    #[test]
    fn destination_point_offsets_by_bearing() {
        let center = GeoPoint {
            lon: 142.5,
            lat: 15.1,
        };
        let d_km = 111.32; // exactly 1° of latitude
        // Due north: +1° lat, longitude essentially unchanged.
        let n = destination_point(center, 0.0, d_km);
        assert!((n.lat - 16.1).abs() < 0.02, "north lat={}", n.lat);
        assert!((n.lon - 142.5).abs() < 0.02, "north lon={}", n.lon);
        // Due east: latitude ~unchanged, longitude grows by 1°/cos(lat).
        let e = destination_point(center, 90.0, d_km);
        assert!((e.lat - 15.1).abs() < 0.02, "east lat={}", e.lat);
        let expect_dlon = 1.0 / 15.1_f32.to_radians().cos();
        assert!(
            (e.lon - (142.5 + expect_dlon)).abs() < 0.03,
            "east lon={} expect~{}",
            e.lon,
            142.5 + expect_dlon
        );
    }

    #[test]
    fn wind_radii_ring_reaches_each_quadrant_radius() {
        let center = GeoPoint {
            lon: 142.5,
            lat: 15.1,
        };
        // Distinct radii per quadrant so we can tell them apart on the ring.
        let radii = WindRadii {
            kt: 34,
            ne_nm: 260.0,
            se_nm: 200.0,
            sw_nm: 100.0,
            nw_nm: 150.0,
        };
        let ring = wind_radii_ring(center, &radii, 8);
        assert!(
            ring.len() > 30 && ring.first() == ring.last(),
            "closed ring"
        );

        // Northeast reach (bearing 45): both lon and lat above center, and the
        // great-circle distance ≈ 260 NM.
        let ne = destination_point(center, 45.0, radii.ne_nm * KM_PER_NM);
        assert!(ne.lon > center.lon && ne.lat > center.lat);
        // Southwest is the tightest quadrant here (100 NM), so its farthest ring
        // point sits closer to the center than the NE farthest point.
        let sw = destination_point(center, 225.0, radii.sw_nm * KM_PER_NM);
        assert!(sw.lon < center.lon && sw.lat < center.lat);
        let ne_span = (ne.lon - center.lon).hypot(ne.lat - center.lat);
        let sw_span = (sw.lon - center.lon).hypot(sw.lat - center.lat);
        assert!(
            ne_span > sw_span,
            "NE (260 NM) reaches farther than SW (100 NM)"
        );

        // Empty radii ⇒ empty ring.
        let none = WindRadii {
            kt: 34,
            ne_nm: 0.0,
            se_nm: 0.0,
            sw_nm: 0.0,
            nw_nm: 0.0,
        };
        assert!(wind_radii_ring(center, &none, 8).is_empty());
    }

    #[test]
    fn danger_area_envelopes_the_whole_34kt_track() {
        // Build the 34-kt danger area from BAVI's analysis + forecast radii —
        // the same inputs the overlay feeds it. It must fan NW from Guam
        // (~145°E) toward Taiwan/the Philippine Sea (~122°E), enclosing every
        // 34-kt gale field along the way.
        let current = parse_jtwc_current_radii(JTWC_WARNING);
        let fc = parse_jtwc_forecast_warning(JTWC_WARNING);
        let current_center = GeoPoint {
            lon: 145.0,
            lat: 14.3,
        }; // WARNING POSITION
        let points = std::iter::once((current_center, current.as_slice()))
            .chain(fc.iter().map(|p| (p.position, p.wind_radii.as_slice())));
        let hull = danger_area_34kt(points);

        assert!(
            hull.len() >= 4 && hull.first() == hull.last(),
            "closed hull"
        );
        let west = hull.iter().fold(f32::INFINITY, |m, p| m.min(p.lon));
        let east = hull.iter().fold(f32::NEG_INFINITY, |m, p| m.max(p.lon));
        let south = hull.iter().fold(f32::INFINITY, |m, p| m.min(p.lat));
        let north = hull.iter().fold(f32::NEG_INFINITY, |m, p| m.max(p.lat));
        assert!(
            east - west > 20.0,
            "danger area spans the basin: {west}..{east}"
        );
        assert!(
            east > 148.0 && west < 120.0,
            "reaches from Guam-ward ({east}) to Taiwan-ward ({west})"
        );
        // Latitude band brackets the track (14°N current → 26°N day-5).
        assert!(south < 13.0 && north > 27.0, "lat band {south}..{north}");
    }

    // --- cone-envelope geometry helpers (tests only) -----------------------

    /// Distance (km) from `x` to the nearest point of an open polyline —
    /// [`capsule_signed_distance_km`] with zero radii is exactly the spherical
    /// point-to-geodesic-segment distance.
    fn dist_to_polyline_km(x: GeoPoint, line: &[GeoPoint]) -> f32 {
        line.windows(2)
            .map(|w| {
                let len = haversine_km(w[0], w[1]);
                if len <= 0.0 {
                    haversine_km(x, w[0])
                } else {
                    let theta = initial_bearing_deg(w[0], w[1]);
                    capsule_signed_distance_km(x, w[0], 0.0, 0.0, theta, len)
                }
            })
            .fold(f32::INFINITY, f32::min)
    }

    /// Max over `boundary`'s edges (sampled every ~10 km) of the distance to
    /// `to` — with `to` a closed ring this is the one-sided Hausdorff
    /// deviation between two boundaries; with `to` a track polyline it is the
    /// "how far does the cone stray from the track" measure. Edge INTERIORS
    /// matter: a convex hull's vertices all lie on uncertainty circles, only
    /// its long chords stray.
    fn max_boundary_distance_km(boundary: &[GeoPoint], to: &[GeoPoint]) -> f32 {
        let mut worst = 0.0f32;
        for w in boundary.windows(2) {
            let len = haversine_km(w[0], w[1]);
            let theta = initial_bearing_deg(w[0], w[1]);
            let steps = (len / 10.0).ceil().max(1.0) as usize;
            for k in 0..=steps {
                let p = destination_point(w[0], theta, len * k as f32 / steps as f32);
                worst = worst.max(dist_to_polyline_km(p, to));
            }
        }
        worst
    }

    /// Even-odd point-in-ring test in the lon/lat plane (`ring` closed).
    fn point_in_ring(x: GeoPoint, ring: &[GeoPoint]) -> bool {
        let (px, py) = (x.lon as f64, x.lat as f64);
        let mut inside = false;
        for w in ring.windows(2) {
            let (x1, y1) = (w[0].lon as f64, w[0].lat as f64);
            let (x2, y2) = (w[1].lon as f64, w[1].lat as f64);
            if (y1 > py) != (y2 > py) && x1 + (py - y1) / (y2 - y1) * (x2 - x1) > px {
                inside = !inside;
            }
        }
        inside
    }

    /// No two non-adjacent edges of the closed ring properly cross (lon/lat
    /// plane; a pure axis scale can't change crossings, so no cos-lat needed).
    fn ring_is_simple(ring: &[GeoPoint]) -> bool {
        let pts: Vec<(f64, f64)> = ring[..ring.len() - 1]
            .iter()
            .map(|p| (p.lon as f64, p.lat as f64))
            .collect();
        let n = pts.len();
        let orient = |p: (f64, f64), q: (f64, f64), r: (f64, f64)| {
            (q.0 - p.0) * (r.1 - p.1) - (q.1 - p.1) * (r.0 - p.0)
        };
        for i in 0..n {
            for j in (i + 1)..n {
                if j == i + 1 || (i == 0 && j == n - 1) {
                    continue; // shared vertex
                }
                let (a, b) = (pts[i], pts[(i + 1) % n]);
                let (c, d) = (pts[j], pts[(j + 1) % n]);
                if orient(a, b, c) * orient(a, b, d) < 0.0
                    && orient(c, d, a) * orient(c, d, b) < 0.0
                {
                    return false;
                }
            }
        }
        true
    }

    /// The pre-fix danger-area construction (convex hull of the sampled 34-kt
    /// roses) — kept here as the comparator the new envelope is judged
    /// against.
    fn legacy_hull_34kt<'a>(
        points: impl Iterator<Item = (GeoPoint, &'a [WindRadii])>,
    ) -> Vec<GeoPoint> {
        let mut hull_input: Vec<GeoPoint> = Vec::new();
        for (center, radii) in points {
            if let Some(r34) = radii.iter().find(|r| r.kt == 34) {
                hull_input.extend(wind_radii_ring(center, r34, 6));
            }
        }
        if hull_input.len() < 3 {
            return Vec::new();
        }
        convex_hull(&hull_input)
    }

    /// On a straight track the tapered envelope IS the old hull shape (tangent
    /// trapezoids + end arcs): the fix must not move straight-track cones
    /// beyond arc discretization. Radii taper hard (100→200 NM over ~3°), so
    /// this also pins the true external-tangent tilt — a perpendicular-offset
    /// approximation would sit ~7 km inside the hull at the small end and fail
    /// the 5-km bound.
    #[test]
    fn straight_track_envelope_matches_the_legacy_hull() {
        let discs = [
            (
                GeoPoint {
                    lon: 130.0,
                    lat: 0.0,
                },
                100.0f32,
            ),
            (
                GeoPoint {
                    lon: 133.0,
                    lat: 0.0,
                },
                150.0,
            ),
            (
                GeoPoint {
                    lon: 136.0,
                    lat: 0.0,
                },
                200.0,
            ),
        ];
        let envelope = track_circle_envelope(&discs);
        assert!(envelope.len() >= 4 && envelope.first() == envelope.last());
        // The legacy shape, sampled finely (5°) so the comparison tolerance is
        // dominated by the new ring's own 10° arcs, not the reference's.
        let mut hull_input = Vec::new();
        for (center, r_nm) in discs {
            for k in 0..72 {
                hull_input.push(destination_point(center, 5.0 * k as f32, r_nm * KM_PER_NM));
            }
        }
        let hull = convex_hull(&hull_input);
        let dev = max_boundary_distance_km(&envelope, &hull)
            .max(max_boundary_distance_km(&hull, &envelope));
        assert!(dev < 5.0, "straight-track shape preserved, dev {dev} km");
    }

    /// The owner-reported v0.29.2 bug, on the real recurving Bavi warning #25:
    /// the convex-hull cone straight-lined across the bend (inner side cut,
    /// front fat), while the tapered envelope must hug the curved track —
    /// every boundary point within the largest gale radius (+ miter slack) of
    /// the forecast polyline — while still containing every per-point circle,
    /// without self-intersecting.
    #[test]
    fn curved_bavi25_envelope_hugs_the_recurving_track() {
        let current = parse_jtwc_current_radii(JTWC_WARNING_25);
        let fc = parse_jtwc_forecast_warning(JTWC_WARNING_25);
        let analysis = GeoPoint {
            lon: 139.9,
            lat: 16.2,
        }; // WARNING POSITION 070000Z
        let track_points: Vec<(GeoPoint, &[WindRadii])> =
            std::iter::once((analysis, current.as_slice()))
                .chain(fc.iter().map(|p| (p.position, p.wind_radii.as_slice())))
                .collect();
        let discs: Vec<(GeoPoint, f32)> = track_points
            .iter()
            .filter_map(|(c, radii)| {
                radii
                    .iter()
                    .find(|r| r.kt == 34)
                    .map(|r| (*c, r.max_nm() * KM_PER_NM))
            })
            .collect();
        assert_eq!(
            discs.len(),
            9,
            "analysis + 8 forecast points carry 34-kt radii"
        );
        let polyline: Vec<GeoPoint> = discs.iter().map(|d| d.0).collect();
        let max_r = discs.iter().fold(0.0f32, |m, d| m.max(d.1)); // 270 NM

        let envelope = danger_area_34kt(track_points.iter().copied());
        assert!(envelope.len() >= 4 && envelope.first() == envelope.last());
        assert!(ring_is_simple(&envelope), "no self-intersection");

        // Hugs the curve: nothing strays past the biggest circle (1 % miter
        // slack for the ≤11° per-vertex turns of this track).
        let dev = max_boundary_distance_km(&envelope, &polyline);
        assert!(
            dev <= max_r * 1.01 + 2.0,
            "envelope hugs the recurve: {dev} km vs max radius {max_r} km"
        );
        // ... which the legacy hull genuinely violated — its inner-bend chord
        // ran ~150 km outside any circle. This is the regression the fix
        // removes; if it ever fails the fixture stopped recurving.
        let hull = legacy_hull_34kt(track_points.iter().copied());
        let hull_dev = max_boundary_distance_km(&hull, &polyline);
        assert!(
            hull_dev > max_r + 80.0,
            "fixture still recurves enough to expose the hull bug ({hull_dev} km)"
        );

        // Still a cone: every per-point gale circle is inside (sampled 3 km in
        // from the rim to absorb the 10° arc chords).
        for (center, r_km) in &discs {
            for k in 0..24 {
                let p = destination_point(*center, 15.0 * k as f32, r_km - 3.0);
                assert!(
                    point_in_ring(p, &envelope),
                    "circle sample {k} of {center:?} escaped the envelope"
                );
            }
        }
    }

    /// A sharp 90° bend whose radii rival the segment lengths: the inner-side
    /// offsets cross, and the miter + interior prune must keep the boundary
    /// simple while every circle stays contained.
    #[test]
    fn envelope_inner_bend_stays_simple_and_contains_circles() {
        let discs = [
            (
                GeoPoint {
                    lon: 130.0,
                    lat: 10.0,
                },
                120.0f32,
            ),
            (
                GeoPoint {
                    lon: 133.0,
                    lat: 10.0,
                },
                140.0,
            ),
            (
                GeoPoint {
                    lon: 133.0,
                    lat: 13.0,
                },
                160.0,
            ),
        ];
        let ring = track_circle_envelope(&discs);
        assert!(ring.len() >= 4 && ring.first() == ring.last());
        assert!(ring_is_simple(&ring), "inner bend must not self-loop");
        for (center, r_nm) in discs {
            let r_km = r_nm * KM_PER_NM;
            for k in 0..24 {
                let p = destination_point(center, 15.0 * k as f32, r_km - 6.0);
                assert!(
                    point_in_ring(p, &ring),
                    "circle sample {k} of {center:?} escaped the bend envelope"
                );
            }
        }
        // The 90° corner's miter may stand off up to r/cos(45°) from the
        // vertex; beyond that everything must still hug the track.
        let polyline = [discs[0].0, discs[1].0, discs[2].0];
        let dev = max_boundary_distance_km(&ring, &polyline);
        assert!(dev <= 160.0 * KM_PER_NM * 1.45, "sharp-bend hug: {dev} km");

        // S-curve stress: alternating bends with segments SHORTER than the
        // radii, so inner offsets overrun several neighbors and the
        // trim+prune pair carries the whole boundary.
        let scurve = [
            (
                GeoPoint {
                    lon: 130.0,
                    lat: 10.0,
                },
                150.0f32,
            ),
            (
                GeoPoint {
                    lon: 131.5,
                    lat: 10.0,
                },
                160.0,
            ),
            (
                GeoPoint {
                    lon: 132.5,
                    lat: 11.0,
                },
                170.0,
            ),
            (
                GeoPoint {
                    lon: 132.5,
                    lat: 12.5,
                },
                180.0,
            ),
            (
                GeoPoint {
                    lon: 131.5,
                    lat: 13.5,
                },
                190.0,
            ),
            (
                GeoPoint {
                    lon: 130.0,
                    lat: 13.5,
                },
                200.0,
            ),
        ];
        let ring = track_circle_envelope(&scurve);
        assert!(ring.len() >= 4 && ring.first() == ring.last());
        assert!(ring_is_simple(&ring), "S-curve must not self-loop");
        for (center, r_nm) in scurve {
            let r_km = r_nm * KM_PER_NM;
            for k in 0..24 {
                let p = destination_point(center, 15.0 * k as f32, r_km - 8.0);
                assert!(
                    point_in_ring(p, &ring),
                    "S-curve circle sample {k} of {center:?} escaped"
                );
            }
        }
    }

    /// A single-point "track" is just that point's uncertainty circle.
    #[test]
    fn envelope_single_point_is_the_circle() {
        let center = GeoPoint {
            lon: 140.0,
            lat: 20.0,
        };
        let ring = track_circle_envelope(&[(center, 150.0)]);
        assert!(ring.len() >= 24 && ring.first() == ring.last());
        for p in &ring {
            let d = haversine_km(*p, center);
            assert!((d - 150.0 * KM_PER_NM).abs() < 1.0, "on the circle: {d} km");
        }
        assert!(track_circle_envelope(&[]).is_empty());
        assert!(track_circle_envelope(&[(center, 0.0)]).is_empty());
    }

    /// Duplicate / near-duplicate forecast positions and a point whose circle
    /// is swallowed by its neighbor's must all collapse to the clean two-point
    /// cone instead of spiking the boundary with noise bearings.
    #[test]
    fn envelope_merges_duplicate_and_swallowed_points() {
        let a = GeoPoint {
            lon: 130.0,
            lat: 15.0,
        };
        let b = GeoPoint {
            lon: 133.0,
            lat: 15.0,
        };
        let clean = track_circle_envelope(&[(a, 150.0), (b, 180.0)]);
        assert!(ring_is_simple(&clean));

        let jitter = GeoPoint {
            lon: 130.001,
            lat: 15.0,
        };
        let with_dups =
            track_circle_envelope(&[(a, 150.0), (a, 150.0), (jitter, 150.0), (b, 180.0)]);
        assert!(ring_is_simple(&with_dups));
        let dev = max_boundary_distance_km(&with_dups, &clean)
            .max(max_boundary_distance_km(&clean, &with_dups));
        assert!(dev < 2.0, "duplicates change nothing: {dev} km");

        // 12 NM from `a` with a 120-NM radius: entirely inside a's 150-NM
        // circle, so it must merge away rather than kink the taper.
        let swallowed = GeoPoint {
            lon: 130.2,
            lat: 15.0,
        };
        let with_contained = track_circle_envelope(&[(a, 150.0), (swallowed, 120.0), (b, 180.0)]);
        assert!(ring_is_simple(&with_contained));
        let dev = max_boundary_distance_km(&with_contained, &clean)
            .max(max_boundary_distance_km(&clean, &with_contained));
        assert!(dev < 2.0, "a swallowed circle changes nothing: {dev} km");
    }

    #[test]
    fn nhc_tcm_parses_per_point_wind_radii() {
        // Audit #3 regression: the TCM's quadrant radii were deliberately
        // skipped, so NHC storms rendered as bare dots. Exact values from the
        // real Milton (AL142024) Forecast/Advisory #15 fixture.
        let fc = parse_nhc_forecast_advisory(NHC_TCM);
        assert_eq!(fc.len(), 8);

        // First point (09/0600Z): all three thresholds, strongest-first.
        let first = &fc[0];
        assert_eq!(
            first.wind_radii.iter().map(|r| r.kt).collect::<Vec<_>>(),
            vec![64, 50, 34]
        );
        let r64 = first.wind_radii.iter().find(|r| r.kt == 64).unwrap();
        assert_eq!(
            (r64.ne_nm, r64.se_nm, r64.sw_nm, r64.nw_nm),
            (30.0, 25.0, 25.0, 30.0)
        );
        let r50 = first.wind_radii.iter().find(|r| r.kt == 50).unwrap();
        assert_eq!(
            (r50.ne_nm, r50.se_nm, r50.sw_nm, r50.nw_nm),
            (60.0, 50.0, 45.0, 50.0)
        );
        let r34 = first.wind_radii.iter().find(|r| r.kt == 34).unwrap();
        assert_eq!(
            (r34.ne_nm, r34.se_nm, r34.sw_nm, r34.nw_nm),
            (100.0, 100.0, 80.0, 120.0),
            "the dots-abut-the-radius form `34 KT...100NE` parses"
        );

        // 11/0600Z (65 kt, weakening): the 64-kt threshold is genuinely gone.
        assert_eq!(
            fc[4].wind_radii.iter().map(|r| r.kt).collect::<Vec<_>>(),
            vec![50, 34]
        );

        // Day-5 outlook point: 34-kt only, with a genuine zero NE radius.
        let last = fc.last().unwrap();
        assert_eq!(
            last.wind_radii.iter().map(|r| r.kt).collect::<Vec<_>>(),
            vec![34]
        );
        let l34 = &last.wind_radii[0];
        assert_eq!(
            (l34.ne_nm, l34.se_nm, l34.sw_nm, l34.nw_nm),
            (0.0, 120.0, 120.0, 120.0)
        );

        // Per-block scoping: the 34-kt NE radius really varies point-to-point,
        // so one block's radii must never bleed into the next.
        let ne34: Vec<f32> = fc
            .iter()
            .map(|p| p.wind_radii.iter().find(|r| r.kt == 34).unwrap().ne_nm)
            .collect();
        assert_eq!(
            ne34,
            vec![100.0, 160.0, 230.0, 240.0, 270.0, 180.0, 130.0, 0.0]
        );
    }

    #[test]
    fn nhc_current_radii_parses_analysis_block_and_skips_seas() {
        let radii = parse_nhc_current_radii(NHC_TCM);
        // Exactly the three wind thresholds — the `12 FT SEAS..` line (same
        // columnar layout, different quantity) must NOT be read as wind radii.
        assert_eq!(
            radii.iter().map(|r| r.kt).collect::<Vec<_>>(),
            vec![64, 50, 34]
        );
        let r64 = radii.iter().find(|r| r.kt == 64).unwrap();
        assert_eq!(
            (r64.ne_nm, r64.se_nm, r64.sw_nm, r64.nw_nm),
            (25.0, 25.0, 25.0, 25.0)
        );
        let r50 = radii.iter().find(|r| r.kt == 50).unwrap();
        assert_eq!(
            (r50.ne_nm, r50.se_nm, r50.sw_nm, r50.nw_nm),
            (40.0, 40.0, 40.0, 40.0)
        );
        let r34 = radii.iter().find(|r| r.kt == 34).unwrap();
        assert_eq!(
            (r34.ne_nm, r34.se_nm, r34.sw_nm, r34.nw_nm),
            (70.0, 80.0, 80.0, 120.0)
        );
    }

    #[test]
    fn nhc_geometry_carries_radii_end_to_end() {
        // What `fetch_storm_geometry(Source::Nhc, ..)` now returns for Milton:
        // per-point + analysis radii, so the existing wind-rose and 34-kt
        // danger-area renderer lights up for NHC storms exactly like the JTWC
        // path (they used to draw as bare dots on a thin line).
        let geom = nhc_geometry_from_forecast_advisory(NHC_TCM);
        assert_eq!(geom.forecast.len(), 8);
        assert!(!geom.current_wind_radii.is_empty());
        assert!(
            geom.forecast
                .iter()
                .all(|p| p.wind_radii.iter().any(|r| r.kt == 34)),
            "every Milton forecast point carries a 34-kt gale radius"
        );
        // And the 34-kt danger area actually forms from those inputs (the
        // overlay's gate): current position 22.7N 87.5W + forecast points.
        let current_center = GeoPoint {
            lon: -87.5,
            lat: 22.7,
        };
        let points = std::iter::once((current_center, geom.current_wind_radii.as_slice())).chain(
            geom.forecast
                .iter()
                .map(|p| (p.position, p.wind_radii.as_slice())),
        );
        let hull = danger_area_34kt(points);
        assert!(
            hull.len() >= 4 && hull.first() == hull.last(),
            "closed danger-area hull"
        );
    }

    /// Live end-to-end smoke check against the real feeds (network; run
    /// manually: `cargo test -p data_source --release -- --ignored live_tropical`).
    /// Fetches + merges NHC/GDACS/JTWC and then every storm's geometry, so a
    /// live format drift in any product is caught before it ships.
    #[test]
    #[ignore]
    fn live_tropical_feeds_end_to_end() {
        let client = reqwest::blocking::Client::builder()
            .user_agent("BowEcho tropical layer (github.com/FahrenheitResearch/bowecho)")
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("client");
        let storms = fetch_active_cyclones(&client).expect("fetch+merge live sources");
        println!("{} active tropical cyclone(s):", storms.len());
        for storm in &storms {
            println!(
                "  {} [{}] {:?} {:.0} kt at ({:.1}, {:.1}) geometry_url={} jtwc={} rss_warning_nr={:?}",
                storm.name,
                storm.source.label(),
                storm.basin,
                storm.max_wind_kt.unwrap_or(0.0),
                storm.position.lon,
                storm.position.lat,
                storm.geometry_url.is_some(),
                storm.forecast_url.is_some(),
                storm.jtwc_warning_nr,
            );
        }
        for storm in &storms {
            let Some(url) = storm.geometry_url.as_deref() else {
                continue;
            };
            let geom =
                fetch_storm_geometry(&client, storm.source, url, storm.forecast_url.as_deref())
                    .unwrap_or_else(|err| panic!("geometry fetch for {}: {err}", storm.name));
            println!(
                "  {}: {} forecast pts ({} with radii), {} current radii, cone {} pts, {} track segs",
                storm.name,
                geom.forecast.len(),
                geom.forecast
                    .iter()
                    .filter(|p| !p.wind_radii.is_empty())
                    .count(),
                geom.current_wind_radii.len(),
                geom.cone.len(),
                geom.track.len(),
            );
            // The staleness fix, proven against the live bulletin: the parsed
            // warning identity + analysis intensity must replace the
            // aggregator's numbers on the storm record.
            if let Some(warning) = &geom.warning {
                let mut synced = storm.clone();
                sync_storm_with_geometry(&mut synced, &geom);
                println!("    bulletin: {}", warning.identity_summary(Utc::now()));
                println!(
                    "    displayed intensity now {:?} kt / {:?} (list source said {:?} kt)",
                    synced.max_wind_kt, synced.classification, storm.max_wind_kt,
                );
                if storm.forecast_url.is_some() {
                    assert!(warning.number.is_some(), "live JTWC warning number parses");
                    assert!(warning.issued.is_some(), "live JTWC issue DTG parses");
                    assert!(
                        warning.max_wind_kt.is_some(),
                        "live JTWC analysis intensity parses"
                    );
                    assert_eq!(
                        synced.max_wind_kt, warning.max_wind_kt,
                        "official analysis intensity replaces the aggregator's"
                    );
                }
            } else if storm.forecast_url.is_some() {
                panic!(
                    "JTWC-matched storm {} fetched without warning identity",
                    storm.name
                );
            }
        }
    }

    #[test]
    fn jtwc_enrichment_carries_radii_end_to_end() {
        // The GDACS getgeometry forecast points carry no radii; the matched JTWC
        // warning supplies both per-point and current radii, exactly as
        // `fetch_storm_geometry` wires them (via `apply_jtwc_warning`).
        let mut geometry = parse_gdacs_geometry(GDACS_FCST).unwrap();
        assert!(geometry.current_wind_radii.is_empty());
        assert!(geometry.forecast.iter().all(|p| p.wind_radii.is_empty()));
        assert!(geometry.warning.is_none());

        apply_jtwc_warning(&mut geometry, JTWC_WARNING);
        assert!(!geometry.current_wind_radii.is_empty());
        assert!(
            geometry
                .forecast
                .iter()
                .all(|p| p.wind_radii.iter().any(|r| r.kt == 34)),
            "every forecast point has a 34-kt gale radius"
        );
        // The warning identity rides along for the card + severity override.
        assert_eq!(geometry.warning.as_ref().and_then(|w| w.number), Some(21));
    }

    #[test]
    fn jtwc_warning_info_parses_identity_and_analysis_vitals() {
        let info = parse_jtwc_warning_info(JTWC_WARNING).expect("warning info");
        assert_eq!(info.agency, WarningAgency::Jtwc);
        assert_eq!(info.number, Some(21));
        // WMO header `WTPN31 PGTW 060300` + REMARKS `06JUL26`.
        assert_eq!(
            info.issued,
            NaiveDate::from_ymd_opt(2026, 7, 6)
                .unwrap()
                .and_hms_opt(3, 0, 0)
                .map(|dt| dt.and_utc())
        );
        // WARNING POSITION: 060000Z --- NEAR 14.3N 145.0E.
        assert_eq!(
            info.position_time,
            NaiveDate::from_ymd_opt(2026, 7, 6)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .map(|dt| dt.and_utc())
        );
        let posit = info.position.expect("analysis position");
        assert!((posit.lat - 14.3).abs() < 1e-3 && (posit.lon - 145.0).abs() < 1e-3);
        // PRESENT WIND DISTRIBUTION: the CURRENT intensity, not a forecast's.
        assert_eq!(info.max_wind_kt, Some(150.0));
        assert_eq!(info.gust_kt, Some(180.0));
        assert_eq!(info.movement_dir_deg, Some(285.0));
        assert_eq!(info.movement_speed_kt, Some(11.0));
        assert_eq!(info.min_pressure_mb, Some(906.0));

        // The live #25 bulletin: analysis wind 125 kt (first MAX SUSTAINED
        // WINDS is the analysis block's, never a forecast block's) and the
        // REMARKS pressure sentence wrapped across a line break still parses.
        let info25 = parse_jtwc_warning_info(JTWC_WARNING_25).expect("warning info");
        assert_eq!(info25.number, Some(25));
        assert_eq!(info25.max_wind_kt, Some(125.0));
        assert_eq!(info25.min_pressure_mb, Some(934.0));
    }

    #[test]
    fn nhc_warning_info_parses_identity_and_analysis_vitals() {
        let info = parse_nhc_warning_info(NHC_TCM).expect("advisory info");
        assert_eq!(info.agency, WarningAgency::Nhc);
        assert_eq!(info.number, Some(15));
        // `2100 UTC TUE OCT 08 2024`.
        assert_eq!(
            info.issued,
            NaiveDate::from_ymd_opt(2024, 10, 8)
                .unwrap()
                .and_hms_opt(21, 0, 0)
                .map(|dt| dt.and_utc())
        );
        // `HURRICANE CENTER LOCATED NEAR 22.7N  87.5W AT 08/2100Z`.
        assert_eq!(info.position_time, info.issued);
        let posit = info.position.expect("analysis position");
        assert!((posit.lat - 22.7).abs() < 1e-3 && (posit.lon + 87.5).abs() < 1e-3);
        assert_eq!(info.max_wind_kt, Some(145.0));
        assert_eq!(info.gust_kt, Some(175.0));
        assert_eq!(info.min_pressure_mb, Some(918.0));
    }

    #[test]
    fn warning_identity_summary_shows_number_issue_age_and_position_time() {
        use chrono::TimeZone;
        // The exact card strings — the user must always see how old the
        // intensity is.
        let jtwc = parse_jtwc_warning_info(JTWC_WARNING_25).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 7, 7, 0, 0).unwrap();
        assert_eq!(
            jtwc.identity_summary(now),
            "JTWC Warning #25 · issued 07/0300Z (4 h ago) · position 07/0000Z"
        );
        let nhc = parse_nhc_warning_info(NHC_TCM).unwrap();
        let now = Utc.with_ymd_and_hms(2024, 10, 8, 23, 30, 0).unwrap();
        assert_eq!(
            nhc.identity_summary(now),
            "NHC Advisory #15 · issued 08/2100Z (2 h ago) · position 08/2100Z"
        );
        assert_eq!(jtwc.product_label(), "JTWC Warning #25");
    }

    /// THE staleness proof (owner report, 2026-07-06): warning N then N+1
    /// through the exact pipeline two geometry refetches run — the stored
    /// storm state must reflect N+1, not the old peak.
    #[test]
    fn newer_jtwc_warning_replaces_stored_vitals() {
        let mut storm = parse_gdacs_event_list(GDACS_LIST)
            .unwrap()
            .into_iter()
            .find(|s| s.name == "Bavi")
            .unwrap();

        // First fetch under Warning #21: the official 150 kt replaces the
        // GDACS ~145-kt severity estimate.
        let mut geometry = parse_gdacs_geometry(GDACS_FCST).unwrap();
        apply_jtwc_warning(&mut geometry, JTWC_WARNING);
        sync_storm_with_geometry(&mut storm, &geometry);
        assert_eq!(storm.warning.as_ref().and_then(|w| w.number), Some(21));
        assert_eq!(storm.max_wind_kt, Some(150.0));
        assert_eq!(storm.classification, "Super Typhoon");

        // Refetch under Warning #25 (live 2026-07-07 product, downgraded):
        // EVERYTHING follows — intensity, category/label, gusts, pressure,
        // motion, analysis position, forecast dots, and the identity line.
        apply_jtwc_warning(&mut geometry, JTWC_WARNING_25);
        sync_storm_with_geometry(&mut storm, &geometry);
        let warning = storm.warning.clone().expect("warning attached");
        assert_eq!(warning.number, Some(25));
        assert_eq!(
            warning.issued,
            NaiveDate::from_ymd_opt(2026, 7, 7)
                .unwrap()
                .and_hms_opt(3, 0, 0)
                .map(|dt| dt.and_utc())
        );
        assert_eq!(storm.max_wind_kt, Some(125.0), "no stale 150/155 kt");
        assert_eq!(storm.category, Some(Category::Four));
        assert_eq!(storm.classification, "Category 4 Typhoon");
        assert_eq!(storm.gust_kt, Some(150.0));
        assert_eq!(storm.min_pressure_mb, Some(934.0));
        assert_eq!(storm.movement_speed_kt, Some(12.0));
        assert!(
            (storm.position.lat - 16.2).abs() < 1e-3 && (storm.position.lon - 139.9).abs() < 1e-3,
            "analysis position follows the newest warning"
        );
        // Forecast dots re-keyed to #25: first point 071200Z at 130 kt.
        assert_eq!(storm.forecast[0].max_wind_kt, Some(130.0));
        assert_eq!(
            storm.forecast[0].valid_time,
            NaiveDate::from_ymd_opt(2026, 7, 7)
                .unwrap()
                .and_hms_opt(12, 0, 0)
                .map(|dt| dt.and_utc())
        );
    }

    #[test]
    fn sync_attaches_nhc_advisory_identity_without_overriding_vitals() {
        // NHC vitals come from CurrentStorms.json (refreshed every public
        // advisory, at least as fresh as the 6-hourly TCM) — the TCM identity
        // is attached for display but must NOT clobber them.
        let mut storm = parse_nhc_current_storms(NHC).unwrap().pop().unwrap(); // Alberto
        let geometry = nhc_geometry_from_forecast_advisory(NHC_TCM);
        sync_storm_with_geometry(&mut storm, &geometry);
        assert_eq!(storm.max_wind_kt, Some(85.0), "CurrentStorms wind kept");
        assert_eq!(storm.classification, "Category 2 Hurricane");
        assert!((storm.position.lat - 24.5).abs() < 1e-3, "position kept");
        let warning = storm.warning.as_ref().expect("advisory identity");
        assert_eq!(warning.agency, WarningAgency::Nhc);
        assert_eq!(warning.number, Some(15));
        // Forecast + radii still mirror through.
        assert_eq!(storm.forecast.len(), 8);
        assert!(!storm.current_wind_radii.is_empty());
    }
}
