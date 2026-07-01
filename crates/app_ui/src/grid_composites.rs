use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Duration, TimeZone, Utc};
use rustwx_core::{CanonicalField, FieldSelector, GridProjection, ModelId, SelectedField2D};
use rustwx_products::viewer::operational_style_for_store_variable;
use rw_store::grid::GridFile;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GridCompositeSource {
    MrmsLowestAltitudeReflectivity,
    MrmsCompositeReflectivity,
    EumetnetOperaDbzh,
}

impl GridCompositeSource {
    pub(crate) const ALL: [Self; 3] = [
        Self::MrmsLowestAltitudeReflectivity,
        Self::MrmsCompositeReflectivity,
        Self::EumetnetOperaDbzh,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::MrmsLowestAltitudeReflectivity => "MRMS lowest-altitude reflectivity",
            Self::MrmsCompositeReflectivity => "MRMS composite reflectivity",
            Self::EumetnetOperaDbzh => "EUMETNET OPERA DBZH composite",
        }
    }

    pub(crate) fn short_label(self) -> &'static str {
        match self {
            Self::MrmsLowestAltitudeReflectivity => "MRMS low REF",
            Self::MrmsCompositeReflectivity => "MRMS CREF",
            Self::EumetnetOperaDbzh => "OPERA DBZH",
        }
    }

    pub(crate) fn model_slug(self) -> &'static str {
        match self {
            Self::MrmsLowestAltitudeReflectivity | Self::MrmsCompositeReflectivity => "mrms",
            Self::EumetnetOperaDbzh => "eumetnet-opera",
        }
    }

    pub(crate) fn variable_slug(self) -> &'static str {
        match self {
            Self::MrmsLowestAltitudeReflectivity => "mrms_reflectivity_lowest_altitude",
            Self::MrmsCompositeReflectivity => "mrms_composite_reflectivity",
            Self::EumetnetOperaDbzh => "eumetnet_opera_dbzh_composite",
        }
    }

    pub(crate) fn from_variable_slug(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|source| source.variable_slug() == value)
    }

    pub(crate) fn map_center(self) -> (f32, f32, f32) {
        match self {
            Self::MrmsLowestAltitudeReflectivity | Self::MrmsCompositeReflectivity => {
                (39.0, -97.0, 26.0)
            }
            Self::EumetnetOperaDbzh => (50.5, 10.0, 22.0),
        }
    }
}

/// One completed fetch: the map-layer field plus the honest data time
/// for the rail row (Italy DPC / Taiwan CWA layers show theirs; grid
/// composites must too).
pub(crate) struct GridCompositeFetch {
    pub(crate) field: rw_ui::FieldData,
    /// Real product valid/reference time when the upstream carries one
    /// (MRMS GRIB2 section-1 reference time; OPERA ODIM filename stamp).
    pub(crate) valid_time: Option<DateTime<Utc>>,
    /// Wall-clock fetch time — the honest fallback label when the
    /// upstream product carries no timestamp.
    pub(crate) fetched_at_utc: DateTime<Utc>,
}

pub(crate) type GridCompositeResult = Result<GridCompositeFetch, String>;

pub(crate) fn load_latest_field(source: GridCompositeSource) -> GridCompositeResult {
    let (selected, valid_time) = match source {
        GridCompositeSource::MrmsLowestAltitudeReflectivity => fetch_mrms_latest(
            "ReflectivityAtLowestAltitude",
            FieldSelector::altitude_msl(CanonicalField::RadarReflectivity, 500),
            "MRMS lowest-altitude reflectivity",
        )?,
        GridCompositeSource::MrmsCompositeReflectivity => fetch_mrms_latest(
            "MergedReflectivityQCComposite",
            FieldSelector::altitude_msl(CanonicalField::CompositeReflectivity, 500),
            "MRMS composite reflectivity",
        )?,
        GridCompositeSource::EumetnetOperaDbzh => fetch_opera_latest()?,
    };
    let field = selected_field_to_field_data(source, selected)?;
    Ok(GridCompositeFetch {
        field,
        valid_time,
        fetched_at_utc: Utc::now(),
    })
}

/// Mirrors `rustwx_io::extract_mrms_latest_*` but keeps the raw GRIB2
/// bytes long enough to read the product's reference time — the engine
/// helpers discard it (`SelectedField2D` carries no timestamp).
fn fetch_mrms_latest(
    product: &str,
    selector: FieldSelector,
    label: &str,
) -> Result<(SelectedField2D, Option<DateTime<Utc>>), String> {
    let bytes = rustwx_io::fetch_mrms_latest_product(product)
        .map_err(|err| format!("{label} failed: {err}"))?;
    let valid_time = grib2_reference_time(&bytes);
    let selected = rustwx_io::extract_field_from_bytes(&bytes, selector)
        .map_err(|err| format!("{label} failed: {err}"))?;
    Ok((selected, valid_time))
}

/// Mirrors `rustwx_io::fetch_eumetnet_opera_latest_dbzh_for_range`, split
/// apart so the ODIM link's filename stamp (`OPERA@YYYYMMDDTHHMM@…`)
/// survives as the frame's valid time.
fn fetch_opera_latest() -> Result<(SelectedField2D, Option<DateTime<Utc>>), String> {
    let end = Utc::now();
    let start = end - Duration::minutes(35);
    let range = format!("{}/{}", format_opera_time(start), format_opera_time(end));
    let coverage = rustwx_io::fetch_eumetnet_opera_dbzh_coverage(&range)
        .map_err(|err| format!("EUMETNET OPERA DBZH failed: {err}"))?;
    let link = coverage
        .latest_odim_link()
        .ok_or_else(|| "EUMETNET OPERA DBZH failed: coverage has no ODIM HDF5 links".to_owned())?;
    let valid_time = opera_link_valid_time(&link.href);
    let bytes = rustwx_io::fetch_eumetnet_opera_odim_h5(&link.href)
        .map_err(|err| format!("EUMETNET OPERA DBZH failed: {err}"))?;
    let selected = rustwx_io::extract_eumetnet_opera_dbzh_from_odim_h5(&bytes)
        .map_err(|err| format!("EUMETNET OPERA DBZH failed: {err}"))?;
    Ok((selected, valid_time))
}

/// Reference time from the first GRIB2 message's identification section
/// (WMO FM 92 GRIB edition 2, Section 1 octets 13-19). MRMS mosaics are
/// observation composites, so the reference time IS the product valid
/// time.
fn grib2_reference_time(bytes: &[u8]) -> Option<DateTime<Utc>> {
    // Section 0 is a fixed 16 octets ("GRIB", reserved, discipline,
    // edition, total length); Section 1 follows immediately.
    if bytes.len() < 35 || &bytes[0..4] != b"GRIB" || bytes[7] != 2 || bytes[20] != 1 {
        return None;
    }
    let section1_len = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    if section1_len < 21 {
        return None;
    }
    let year = i32::from(u16::from_be_bytes([bytes[28], bytes[29]]));
    Utc.with_ymd_and_hms(
        year,
        u32::from(bytes[30]),
        u32::from(bytes[31]),
        u32::from(bytes[32]),
        u32::from(bytes[33]),
        u32::from(bytes[34]),
    )
    .single()
}

/// Valid time from an OPERA ODIM download link — the composite filename
/// carries its nominal stamp (`…/OPERA@20260627T0530@0@DBZH.h5`).
fn opera_link_valid_time(href: &str) -> Option<DateTime<Utc>> {
    let name = href.rsplit('/').next()?;
    let stamp = name
        .split('@')
        .find(|part| part.len() == 13 && part.as_bytes()[8] == b'T')?;
    let naive = chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M").ok()?;
    Some(naive.and_utc())
}

/// Visible grid composite rows refetch on this gate (Italy DPC / Taiwan
/// CWA layers use the same 60-s cadence).
pub(crate) const GRID_COMPOSITE_REFRESH_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(60);

/// Freshness bookkeeping for one grid composite source — feeds the rail
/// row's frame text and the auto-refresh throttle.
#[derive(Clone, Debug)]
pub(crate) struct GridCompositeStatus {
    pub(crate) source: GridCompositeSource,
    pub(crate) valid_time: Option<DateTime<Utc>>,
    pub(crate) fetched_at_utc: Option<DateTime<Utc>>,
    pub(crate) last_refresh_attempt: Option<Instant>,
}

impl GridCompositeStatus {
    /// The rail row's frame text: the product's real valid time, or the
    /// fetch wall time labelled honestly when the upstream carries no
    /// timestamp — never an implied "current".
    pub(crate) fn frame_text(&self) -> Option<String> {
        match (self.valid_time, self.fetched_at_utc) {
            (Some(time), _) => Some(time.format("%H:%MZ").to_string()),
            (None, Some(fetched)) => Some(format!("as fetched {}", fetched.format("%H:%MZ"))),
            (None, None) => None,
        }
    }
}

fn status_entry(
    statuses: &mut Vec<GridCompositeStatus>,
    source: GridCompositeSource,
) -> &mut GridCompositeStatus {
    if let Some(index) = statuses.iter().position(|status| status.source == source) {
        return &mut statuses[index];
    }
    statuses.push(GridCompositeStatus {
        source,
        valid_time: None,
        fetched_at_utc: None,
        last_refresh_attempt: None,
    });
    statuses.last_mut().expect("entry just pushed")
}

pub(crate) fn note_refresh_attempt(
    statuses: &mut Vec<GridCompositeStatus>,
    source: GridCompositeSource,
) {
    status_entry(statuses, source).last_refresh_attempt = Some(Instant::now());
}

pub(crate) fn note_fetch_success(
    statuses: &mut Vec<GridCompositeStatus>,
    source: GridCompositeSource,
    valid_time: Option<DateTime<Utc>>,
    fetched_at_utc: DateTime<Utc>,
) {
    let entry = status_entry(statuses, source);
    entry.valid_time = valid_time;
    entry.fetched_at_utc = Some(fetched_at_utc);
}

pub(crate) fn frame_text_for(
    statuses: &[GridCompositeStatus],
    source: GridCompositeSource,
) -> Option<String> {
    statuses
        .iter()
        .find(|status| status.source == source)
        .and_then(GridCompositeStatus::frame_text)
}

/// The visible source most overdue for a refresh, or `None` while every
/// visible source is inside the 60-s gate. Never-attempted sources sort
/// first, then oldest attempt — multiple visible composites round-robin
/// through the single fetch channel.
pub(crate) fn next_auto_refresh_source(
    visible: impl IntoIterator<Item = GridCompositeSource>,
    statuses: &[GridCompositeStatus],
    now: Instant,
) -> Option<GridCompositeSource> {
    visible
        .into_iter()
        .map(|source| {
            let attempt = statuses
                .iter()
                .find(|status| status.source == source)
                .and_then(|status| status.last_refresh_attempt);
            (attempt, source)
        })
        .filter(|(attempt, _)| {
            attempt.is_none_or(|at| {
                now.saturating_duration_since(at) >= GRID_COMPOSITE_REFRESH_INTERVAL
            })
        })
        .min_by_key(|(attempt, _)| *attempt)
        .map(|(_, source)| source)
}

fn selected_field_to_field_data(
    source: GridCompositeSource,
    selected: rustwx_core::SelectedField2D,
) -> Result<rw_ui::FieldData, String> {
    let nx = selected.grid.shape.nx;
    let ny = selected.grid.shape.ny;
    let grid = Arc::new(GridFile {
        nx,
        ny,
        lat: selected.grid.lat_deg,
        lon: selected.grid.lon_deg,
        projection: selected.projection.clone(),
        hash: grid_identity(source, &selected.projection, nx, ny),
    });
    let lat_descending = grid.lat_descending().unwrap_or(false);
    let selector_json = serde_json::to_value(selected.selector)
        .map_err(|err| format!("grid composite selector encode failed: {err}"))?;
    let mut values = selected.values;
    let style = operational_style_for_store_variable(
        source.variable_slug(),
        &selector_json,
        &selected.units,
        ModelId::Hrrr,
    );
    let units = match &style {
        Some(style) => {
            if !style.convert.is_none() {
                for value in &mut values {
                    *value = style.convert.apply(*value);
                }
            }
            style.display_units.clone()
        }
        None => selected.units,
    };
    sanitize_grid_composite_values(source, &mut values);
    let range = rw_ui::colormap::finite_min_max(&values);
    Ok(rw_ui::FieldData {
        key: rw_ui::FieldKey {
            hour: rw_ui::HourKey {
                model: source.model_slug().to_owned(),
                run: "latest".to_owned(),
                hour: 0,
            },
            var: source.variable_slug().to_owned(),
        },
        units,
        nx,
        ny,
        values,
        range,
        grid: Some(grid),
        lat_descending,
        style,
    })
}

fn sanitize_grid_composite_values(source: GridCompositeSource, values: &mut [f32]) {
    if !matches!(
        source,
        GridCompositeSource::MrmsLowestAltitudeReflectivity
            | GridCompositeSource::MrmsCompositeReflectivity
            | GridCompositeSource::EumetnetOperaDbzh
    ) {
        return;
    }
    for value in values {
        if !value.is_finite() || *value < -25.0 || *value > 95.0 {
            *value = f32::NAN;
        }
    }
}

fn grid_identity(
    source: GridCompositeSource,
    projection: &Option<GridProjection>,
    nx: usize,
    ny: usize,
) -> String {
    format!(
        "{}:{nx}x{ny}:{:?}",
        source.variable_slug(),
        projection.as_ref()
    )
}

fn format_opera_time(time: DateTime<Utc>) -> String {
    time.format("%Y-%m-%dT%H:%MZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustwx_core::{CanonicalField, FieldSelector, GridShape, LatLonGrid, SelectedField2D};

    #[test]
    fn mrms_field_conversion_builds_stable_layer_key() {
        let grid = LatLonGrid::new(
            GridShape { nx: 2, ny: 2 },
            vec![40.0, 40.0, 39.0, 39.0],
            vec![-101.0, -100.0, -101.0, -100.0],
        )
        .unwrap();
        let selected = SelectedField2D::new(
            FieldSelector::altitude_msl(CanonicalField::RadarReflectivity, 500),
            "dBZ",
            grid,
            vec![10.0, 20.0, -32.0, 120.0],
        )
        .unwrap();

        let field = selected_field_to_field_data(
            GridCompositeSource::MrmsLowestAltitudeReflectivity,
            selected,
        )
        .unwrap();

        assert_eq!(field.key.hour.model, "mrms");
        assert_eq!(field.key.var, "mrms_reflectivity_lowest_altitude");
        assert_eq!(field.nx, 2);
        assert_eq!(field.ny, 2);
        assert_eq!(field.range, Some((10.0, 20.0)));
        assert!(field.values[2].is_nan());
        assert!(field.values[3].is_nan());
        assert!(field.grid.is_some());
    }

    /// A minimal GRIB2 header: Section 0 (16 octets) + Section 1 with the
    /// reference time at octets 13-19.
    fn grib2_header(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"GRIB");
        bytes.extend_from_slice(&[0, 0]); // reserved
        bytes.push(209); // discipline (MRMS local)
        bytes.push(2); // edition
        bytes.extend_from_slice(&37u64.to_be_bytes()); // total length
        bytes.extend_from_slice(&21u32.to_be_bytes()); // section 1 length
        bytes.push(1); // section number
        bytes.extend_from_slice(&[0, 161]); // centre
        bytes.extend_from_slice(&[0, 0]); // subcentre
        bytes.push(2); // master tables version
        bytes.push(1); // local tables version
        bytes.push(0); // significance of reference time
        bytes.extend_from_slice(&year.to_be_bytes());
        bytes.extend_from_slice(&[month, day, hour, minute, second]);
        bytes
    }

    #[test]
    fn mrms_grib2_reference_time_is_the_product_valid_time() {
        let bytes = grib2_header(2026, 6, 27, 5, 32, 30);
        assert_eq!(
            grib2_reference_time(&bytes),
            Utc.with_ymd_and_hms(2026, 6, 27, 5, 32, 30).single()
        );
        assert_eq!(grib2_reference_time(&bytes[..20]), None);
        assert_eq!(
            grib2_reference_time(b"not a grib file at all, honest"),
            None
        );
    }

    #[test]
    fn opera_odim_link_stamp_is_the_frame_valid_time() {
        let href = "https://s3.waw3-1.cloudferro.com/openradar-24h/2026/06/27/OPERA/COMP/OPERA@20260627T0530@0@DBZH.h5";
        assert_eq!(
            opera_link_valid_time(href),
            Utc.with_ymd_and_hms(2026, 6, 27, 5, 30, 0).single()
        );
        assert_eq!(
            opera_link_valid_time("https://example.com/no-stamp.h5"),
            None
        );
    }

    #[test]
    fn frame_text_shows_real_valid_time_or_admits_fetch_time() {
        let mut status = GridCompositeStatus {
            source: GridCompositeSource::MrmsCompositeReflectivity,
            valid_time: Utc.with_ymd_and_hms(2026, 6, 27, 5, 32, 30).single(),
            fetched_at_utc: Utc.with_ymd_and_hms(2026, 6, 27, 5, 41, 0).single(),
            last_refresh_attempt: None,
        };
        assert_eq!(status.frame_text().as_deref(), Some("05:32Z"));
        // No timestamp upstream: label the fetch wall time honestly
        // instead of implying a data valid time.
        status.valid_time = None;
        assert_eq!(status.frame_text().as_deref(), Some("as fetched 05:41Z"));
        status.fetched_at_utc = None;
        assert_eq!(status.frame_text(), None);
    }

    #[test]
    fn visible_composites_auto_refresh_on_the_sixty_second_gate() {
        let source = GridCompositeSource::MrmsCompositeReflectivity;
        let now = Instant::now();
        // Never fetched: refresh immediately.
        assert_eq!(next_auto_refresh_source([source], &[], now), Some(source));
        let mut statuses = Vec::new();
        note_refresh_attempt(&mut statuses, source);
        // Just attempted: inside the gate, no refresh (the old behavior
        // NEVER refreshed — the layer decayed behind a manual button).
        assert_eq!(next_auto_refresh_source([source], &statuses, now), None);
        // Past the gate: refresh again.
        let later = now + GRID_COMPOSITE_REFRESH_INTERVAL + std::time::Duration::from_secs(1);
        assert_eq!(
            next_auto_refresh_source([source], &statuses, later),
            Some(source)
        );
        // Two visible sources: the never-attempted one goes first.
        let other = GridCompositeSource::EumetnetOperaDbzh;
        assert_eq!(
            next_auto_refresh_source([source, other], &statuses, later),
            Some(other)
        );
    }
}
