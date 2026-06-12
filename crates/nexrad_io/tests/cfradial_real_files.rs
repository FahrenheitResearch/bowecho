//! Golden-fixture tests for the CfRadial 1.x decoder against REAL radar
//! data — a fixed-site PPI and a mobile DOW8 RHI.
//!
//! Fixture provenance (fetched 2026-06-11; golden values extracted with an
//! independent Python reader — netCDF4 1.7.4 — not with this crate):
//!
//! - `tests/data/cfrad.xsapr_sgp_ppi_20110520.classic.nc`: ARM X-SAPR
//!   (X-band scanning ARM precipitation radar) PPI at the SGP site,
//!   2011-05-20 10:54:16 UTC, CF/Radial 1.2, 40 rays x 42 gates,
//!   reflectivity_horizontal. Source: ARM-DOE/pyart
//!   `pyart/testing/data/example_cfradial_ppi.nc` (BSD-3-Clause; itself
//!   gate/ray-decimated from the full X-SAPR file by Py-ART's
//!   `make_small_cfradial_ppi.py`). CONTAINER CONVERSION: the published
//!   file is netCDF-4; converted to NETCDF3_CLASSIC (CDF-1) with a raw
//!   variable-for-variable copy (netCDF4-python, no mask/scale applied) —
//!   identical data, classic container. Wild CfRadial 1.x is
//!   netCDF-4-dominant today; the classic container is what early Radx
//!   wrote and what this decoder reads.
//! - `tests/data/cfrad.20211011_223602_DOW8_RHI.trim3.nc`: FARM facility
//!   DOW8 truck NATIVE RHI near Urbana, Illinois, 2021-10-11 22:36:02 UTC,
//!   CF-Radial-1.4 (Radx), 148 rays sweeping -0.73 deg to 70 deg elevation
//!   at fixed ~184 deg azimuth, 950 gates at 125 m. Source:
//!   openradar/open-radar-data
//!   `cfrad.20211011_223602.712_to_20211011_223612.091_DOW8_RHI.nc` (MIT).
//!   Same netCDF-4 -> CDF-1 raw conversion; TRIMMED: of the 8 field
//!   variables only DBZHC/VEL/WIDTH are kept (drops DBMHC, NCP, SNRHC,
//!   VL1, VS1) to stay under the fixture size budget. Kept fields are
//!   byte-identical to the published file.
//! - `tests/data/cfrad.xsapr_sgp_ppi_20110520.netcdf4.nc`: the SAME Py-ART
//!   file in its published netCDF-4 container (HDF5 superblock v2,
//!   unmodified bytes) — pins the routing + guidance for the wild-file
//!   case the classic decoder cannot read.

use chrono::{TimeZone, Utc};
use radar_core::{MomentType, ScanMode};

const XSAPR_PPI: &[u8] = include_bytes!("data/cfrad.xsapr_sgp_ppi_20110520.classic.nc");
const DOW8_RHI: &[u8] = include_bytes!("data/cfrad.20211011_223602_DOW8_RHI.trim3.nc");

fn assert_close(actual: f32, expected: f32, tolerance: f32, what: &str) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{what}: {actual} != {expected} (tolerance {tolerance})"
    );
}

#[test]
fn real_xsapr_ppi_decodes_site_geometry_and_gates() {
    assert!(nexrad_io::cfradial::looks_like_netcdf3_bytes(XSAPR_PPI));
    let volume =
        nexrad_io::cfradial::decode_cfradial1_volume(XSAPR_PPI).expect("decode X-SAPR PPI");

    assert_eq!(volume.site.id, "xsapr-sgp");
    assert_close(volume.site.latitude_deg.unwrap(), 36.4908, 1e-4, "lat");
    assert_close(volume.site.longitude_deg.unwrap(), -97.5942, 1e-4, "lon");
    assert_close(volume.site.elevation_m.unwrap(), 214.0, 1e-3, "alt");
    assert_eq!(
        volume.volume_time,
        Utc.with_ymd_and_hms(2011, 5, 20, 10, 54, 16).unwrap()
    );
    assert_eq!(volume.metadata.scan_mode, Some(ScanMode::Ppi));

    assert_eq!(volume.cuts.len(), 1);
    let cut = &volume.cuts[0];
    assert_close(cut.elevation_deg, 0.5, 1e-3, "fixed angle");
    assert_eq!(cut.radials.len(), 40);
    assert_close(cut.radials[0].azimuth_deg, 359.9368, 1e-3, "az0");
    assert_close(cut.radials[0].elevation_deg, 0.4834, 1e-3, "el0");
    assert_close(
        cut.radials[0].nyquist_velocity_mps.expect("nyquist"),
        17.2205,
        1e-3,
        "nyq",
    );
    // Py-ART's decimation left range centers 0, 960, ... -> derived start
    // is -480 m (center minus half a gate). Faithful to the file.
    let gates = &cut.radials[0].gate_range;
    assert_eq!(
        (gates.first_gate_m, gates.gate_spacing_m, gates.gate_count),
        (-480, 960, 42)
    );

    // Field name "reflectivity_horizontal" has no canonical stem: stays an
    // Unknown moment under its CF name rather than guessing.
    let reflectivity = cut
        .moments
        .get(&MomentType::Unknown("reflectivity_horizontal".to_owned()))
        .expect("reflectivity_horizontal");
    assert_close(
        reflectivity.scaled_value(0, 0).unwrap(),
        -6.05,
        1e-3,
        "v[0,0]",
    );
    assert_close(
        reflectivity.scaled_value(0, 21).unwrap(),
        23.30,
        1e-3,
        "v[0,21]",
    );
    assert_close(
        reflectivity.scaled_value(10, 14).unwrap(),
        25.23,
        1e-3,
        "v[10,14]",
    );
    assert_close(
        reflectivity.scaled_value(20, 10).unwrap(),
        20.54,
        1e-3,
        "v[20,10]",
    );
    assert_close(
        reflectivity.scaled_value(39, 41).unwrap(),
        19.68,
        1e-3,
        "v[39,41]",
    );
}

#[test]
fn real_dow8_rhi_decodes_scan_mode_geometry_and_gates() {
    assert!(nexrad_io::cfradial::looks_like_netcdf3_bytes(DOW8_RHI));
    let volume = nexrad_io::cfradial::decode_cfradial1_volume(DOW8_RHI).expect("decode DOW8 RHI");

    // Mobile platform: latitude/longitude are (time) arrays; first sample.
    assert_eq!(volume.site.id, "DOW8");
    assert_eq!(volume.site.name.as_deref(), Some("ILLINOIS"));
    assert_close(volume.site.latitude_deg.unwrap(), 40.0148, 1e-4, "lat");
    assert_close(volume.site.longitude_deg.unwrap(), -88.3318, 1e-4, "lon");
    assert_close(volume.site.elevation_m.unwrap(), 214.0, 0.5, "alt");
    assert_eq!(
        volume.volume_time,
        Utc.with_ymd_and_hms(2021, 10, 11, 22, 36, 2).unwrap()
    );
    // sweep_mode = "rhi" must surface as ScanMode::Rhi.
    assert_eq!(volume.metadata.scan_mode, Some(ScanMode::Rhi));

    assert_eq!(volume.cuts.len(), 1);
    let cut = &volume.cuts[0];
    // RHI convention: the fixed angle is the pointing AZIMUTH (184 deg).
    assert_close(cut.elevation_deg, 184.0, 1e-3, "fixed azimuth");
    assert_eq!(cut.radials.len(), 148);

    // Rays sweep in elevation at near-constant azimuth.
    assert_close(cut.radials[0].azimuth_deg, 182.1149, 1e-3, "az0");
    assert_close(cut.radials[0].elevation_deg, 1.5, 1e-3, "el0");
    assert_close(cut.radials[147].elevation_deg, 70.0, 1e-3, "el147");
    let (mut el_min, mut el_max) = (f32::INFINITY, f32::NEG_INFINITY);
    for radial in &cut.radials {
        el_min = el_min.min(radial.elevation_deg);
        el_max = el_max.max(radial.elevation_deg);
        assert!(
            (182.0..=184.2).contains(&radial.azimuth_deg),
            "azimuth fixed"
        );
    }
    assert_close(el_min, -0.7306, 1e-3, "el min");
    assert_close(el_max, 70.0, 1e-3, "el max");

    // Gate geometry from the range coordinate: centers 62.46, 187.37, ...
    let gates = &cut.radials[0].gate_range;
    assert_eq!(
        (gates.first_gate_m, gates.gate_spacing_m, gates.gate_count),
        (0, 125, 950)
    );
    assert_close(
        cut.radials[0].nyquist_velocity_mps.expect("nyquist"),
        19.8275,
        1e-3,
        "nyq",
    );
    // Ray times: 0.712 s and 10.091 s offsets from time_coverage_start.
    assert_eq!(cut.radials[0].time_offset_ms, 711);
    assert_eq!(cut.radials[147].time_offset_ms, 10091);

    // Golden gates from the independent netCDF4 reader (DBZHC -> REF,
    // VEL -> VEL, WIDTH -> SW via the shared canonical-name stems).
    let reflectivity = cut.moments.get(&MomentType::Reflectivity).expect("REF");
    assert_close(
        reflectivity.scaled_value(0, 0).unwrap(),
        -2.48,
        1e-3,
        "REF[0,0]",
    );
    assert_close(
        reflectivity.scaled_value(0, 475).unwrap(),
        0.26,
        1e-3,
        "REF[0,475]",
    );
    assert_close(
        reflectivity.scaled_value(37, 316).unwrap(),
        0.79,
        1e-3,
        "REF[37,316]",
    );
    assert_close(
        reflectivity.scaled_value(74, 10).unwrap(),
        -23.30,
        1e-3,
        "REF[74,10]",
    );
    assert!(
        reflectivity.scaled_value(147, 949).is_none_or(f32::is_nan),
        "REF[147,949] fill"
    );

    let velocity = cut.moments.get(&MomentType::Velocity).expect("VEL");
    assert_close(velocity.scaled_value(0, 0).unwrap(), 0.91, 1e-3, "VEL[0,0]");
    assert_close(
        velocity.scaled_value(0, 475).unwrap(),
        -16.56,
        1e-3,
        "VEL[0,475]",
    );
    assert_close(
        velocity.scaled_value(147, 949).unwrap(),
        -5.09,
        1e-3,
        "VEL[147,949]",
    );

    let width = cut.moments.get(&MomentType::SpectrumWidth).expect("SW");
    assert_close(width.scaled_value(0, 475).unwrap(), 4.37, 1e-3, "SW[0,475]");
}

/// The PUBLISHED Py-ART file before container conversion: netCDF-4, i.e.
/// HDF5 superblock v2. This is what users will actually drop on the app.
const XSAPR_PPI_NETCDF4: &[u8] = include_bytes!("data/cfrad.xsapr_sgp_ppi_20110520.netcdf4.nc");

#[test]
fn netcdf4_cfradial_routes_to_hdf5_and_gets_conversion_guidance() {
    // The HDF5 signature must never sniff as netCDF3 — netCDF-4 CfRadial
    // routes to the HDF5/ODIM side (same precedence as the app's sniffer).
    assert!(!nexrad_io::cfradial::looks_like_netcdf3_bytes(
        XSAPR_PPI_NETCDF4
    ));
    assert!(nexrad_io::odim::looks_like_hdf5_bytes(XSAPR_PPI_NETCDF4));
    assert!(!nexrad_io::odim::looks_like_hdf5_bytes(XSAPR_PPI));
    assert!(!nexrad_io::dorade::looks_like_dorade_bytes(DOW8_RHI));

    // The explicit error must tell a CfRadial user the fix that works.
    let err = nexrad_io::odim::decode_odim_h5_volume(XSAPR_PPI_NETCDF4).unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("netCDF-4 CfRadial") && message.contains("nccopy -k classic"),
        "unhelpful netCDF-4 error: {message}"
    );
}
