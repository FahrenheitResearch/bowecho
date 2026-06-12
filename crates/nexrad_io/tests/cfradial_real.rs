//! Golden-fixture test for the CfRadial 1.x decoder.
//!
//! Fixture provenance: `tests/data/cfrad_synth.nc` is SYNTHETIC, generated
//! by `tests/data/gen_cfradial_fixture.py` with the netCDF4 python library
//! (1.7.4) in NETCDF3_CLASSIC (CDF-1) format following CfRadial 1.4
//! (Dixon and Lee, NCAR/EOL, 2016). `time` is UNLIMITED, so every per-ray
//! and field variable exercises the record-interleaved data path. Values
//! are deterministic ramps: REF[t,r] = t + 0.5·r dBZ (float, one forced
//! fill), VEL raw[t,r] = 10·t + r packed as shorts with scale 0.01,
//! offset −0.5.

use chrono::{TimeZone, Utc};
use radar_core::{MomentType, ScanMode};

const FIXTURE: &[u8] = include_bytes!("data/cfrad_synth.nc");

#[test]
fn decodes_synthetic_cfradial1_volume() {
    assert!(nexrad_io::cfradial::looks_like_netcdf3_bytes(FIXTURE));
    let volume =
        nexrad_io::cfradial::decode_cfradial1_volume(FIXTURE).expect("decode CfRadial fixture");

    assert_eq!(volume.site.id, "SYNTH1");
    assert_eq!(volume.site.name.as_deref(), Some("Synthetic Pad"));
    assert_eq!(volume.site.latitude_deg, Some(39.74));
    assert_eq!(volume.site.longitude_deg, Some(-103.2927));
    assert_eq!(volume.site.elevation_m, Some(1519.0));
    assert_eq!(
        volume.volume_time,
        Utc.with_ymd_and_hms(2026, 6, 9, 5, 51, 0).unwrap()
    );
    assert_eq!(volume.metadata.scan_mode, Some(ScanMode::Ppi));
    assert_eq!(volume.metadata.decoded_radial_count, 24);

    assert_eq!(volume.cuts.len(), 2);
    assert_eq!(volume.cuts[0].elevation_deg, 0.5);
    assert_eq!(volume.cuts[1].elevation_deg, 1.5);

    let cut = &volume.cuts[0];
    assert_eq!(cut.radials.len(), 12);
    assert_eq!(cut.radials[0].azimuth_deg, 15.0);
    assert_eq!(cut.radials[3].azimuth_deg, 105.0);
    assert_eq!(cut.radials[0].elevation_deg, 0.5);
    // range centers start at 125 m with 250 m spacing → gate start 0 m.
    assert_eq!(cut.radials[0].gate_range.first_gate_m, 0);
    assert_eq!(cut.radials[0].gate_range.gate_spacing_m, 250);
    assert_eq!(cut.radials[0].gate_range.gate_count, 20);
    assert_eq!(cut.radials[0].nyquist_velocity_mps, Some(26.4));
    // time(time) = 0.5 s steps from time_coverage_start.
    assert_eq!(cut.radials[2].time_offset_ms, 1000);

    // REF (float): value = t + 0.5·r.
    let reflectivity = cut.moments.get(&MomentType::Reflectivity).expect("REF");
    assert_eq!(reflectivity.scaled_value(0, 2), Some(1.0)); // t=0, r=2
    assert_eq!(reflectivity.scaled_value(5, 4), Some(7.0)); // t=5, r=4
    // VEL (packed short): physical = raw·0.01 − 0.5, raw = 10·t + r.
    let velocity = cut.moments.get(&MomentType::Velocity).expect("VEL");
    assert_eq!(velocity.scaled_value(0, 0), Some(-0.5));
    let sampled = velocity.scaled_value(3, 7).expect("VEL t=3 r=7");
    assert!((sampled - (37.0 * 0.01 - 0.5)).abs() < 1.0e-4);

    // Sweep 2 (rays 12-23): REF[14,3] was forced to _FillValue. F32 moment
    // storage carries fill as NaN (same convention as DORADE float fields).
    let upper = &volume.cuts[1];
    let upper_ref = upper.moments.get(&MomentType::Reflectivity).expect("REF");
    assert!(
        upper_ref.scaled_value(2, 3).is_none_or(f32::is_nan) // global ray 14
    );
    assert_eq!(upper_ref.scaled_value(2, 4), Some(16.0)); // 14 + 0.5·4
    assert_eq!(upper.radials[0].elevation_deg, 1.5);
}

#[test]
fn level2_decoder_is_not_fooled_by_netcdf_magic() {
    // The router must send CDF files here, not to the Archive II path; the
    // sniffers must be mutually exclusive on this fixture.
    assert!(!nexrad_io::odim::looks_like_hdf5_bytes(FIXTURE));
    assert!(!nexrad_io::dorade::looks_like_dorade_bytes(FIXTURE));
}
