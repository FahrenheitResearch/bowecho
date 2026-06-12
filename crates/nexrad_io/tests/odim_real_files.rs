//! Golden-fixture tests for the ODIM_H5 decoder against REAL operational
//! polar volumes (three writers, three HDF5 feature mixes).
//!
//! Fixture provenance (all fetched 2026-06-11; expected values extracted
//! with an independent Python reader — h5py 3.15.1 — not with this crate):
//!
//! - `tests/data/bejab.pvol.hdf`: RMI Belgium Jabbeke C-band PVOL,
//!   2019-06-06 00:00:22 UTC, 11 DBZH sweeps, H5rad 2.0, superblock v0,
//!   gzip-chunked u8 planes. From wradlib/wradlib-data (MIT),
//!   `data/hdf5/bejab.pvol.hdf`.
//! - `tests/data/20130429043000.rad.bewid.pvol.dbzh.scan1.hdf`: RMI Belgium
//!   Wideumont PVOL, 2013-04-29 04:30 UTC, 5 DBZH sweeps, H5rad 2.1 with
//!   VARIABLE-LENGTH string attributes (global-heap path) and a root
//!   /how NI. From wradlib/wradlib-data (MIT).
//! - `tests/data/T_PAGZ35_C_ENMI_20170421090837.hdf`: met.no Røst (norst)
//!   PVOL, 2017-04-21 09:08:37 UTC, 6 DBZH sweeps, H5rad 2.2 with
//!   SUPERBLOCK VERSION 1 and a 720-ray half-degree lowest sweep. From
//!   openradar/open-radar-data (MIT).

use chrono::{TimeZone, Utc};
use radar_core::{MomentType, ScanMode};

const BEJAB: &[u8] = include_bytes!("data/bejab.pvol.hdf");
const BEWID: &[u8] = include_bytes!("data/20130429043000.rad.bewid.pvol.dbzh.scan1.hdf");
const NORST: &[u8] = include_bytes!("data/T_PAGZ35_C_ENMI_20170421090837.hdf");

fn assert_close(actual: f32, expected: f32, tolerance: f32, what: &str) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{what}: {actual} != {expected} (tolerance {tolerance})"
    );
}

#[test]
fn real_bejab_pvol_decodes_site_geometry_and_gates() {
    assert!(nexrad_io::odim::looks_like_hdf5_bytes(BEJAB));
    let volume = nexrad_io::odim::decode_odim_h5_volume(BEJAB).expect("decode bejab");

    // source = "WMO:06410,RAD:BX42,PLC:Jabbeke,NOD:bejab,..." — NOD wins.
    assert_eq!(volume.site.id, "BEJAB");
    assert_eq!(volume.site.name.as_deref(), Some("Jabbeke"));
    assert_close(volume.site.latitude_deg.unwrap(), 51.1917, 1e-4, "lat");
    assert_close(volume.site.longitude_deg.unwrap(), 3.0642, 1e-4, "lon");
    assert_close(volume.site.elevation_m.unwrap(), 50.0, 1e-3, "height");
    assert_eq!(
        volume.volume_time,
        Utc.with_ymd_and_hms(2019, 6, 6, 0, 0, 22).unwrap()
    );
    assert_eq!(volume.metadata.scan_mode, Some(ScanMode::Ppi));
    assert_eq!(volume.cuts.len(), 11);
    assert_eq!(volume.metadata.decoded_radial_count, 3960);

    // Lowest sweep: 0.3 deg, 360 rays x 598 gates, 500 m spacing from 0 km.
    let cut = &volume.cuts[0];
    assert_close(cut.elevation_deg, 0.3, 1e-5, "elangle");
    assert_eq!(cut.radials.len(), 360);
    assert_close(cut.radials[0].azimuth_deg, 0.5, 1e-5, "az0");
    let gates = &cut.radials[0].gate_range;
    assert_eq!(
        (gates.first_gate_m, gates.gate_spacing_m, gates.gate_count),
        (0, 500, 598)
    );

    // DBZH golden gates (h5py: phys = 0.5*raw - 32; 0=undetect, 255=nodata).
    let dbzh = cut.moments.get(&MomentType::Reflectivity).expect("DBZH");
    assert_close(dbzh.scaled_value(0, 0).unwrap(), 22.5, 1e-4, "v[0,0]");
    assert_close(dbzh.scaled_value(0, 299).unwrap(), 33.5, 1e-4, "v[0,299]");
    assert_eq!(dbzh.scaled_value(90, 199), None, "v[90,199] undetect");
    assert_close(dbzh.scaled_value(180, 10).unwrap(), 28.5, 1e-4, "v[180,10]");
    assert_close(
        dbzh.scaled_value(359, 597).unwrap(),
        18.5,
        1e-4,
        "v[359,597]",
    );

    // Top sweep changes geometry (25 deg, 300 gates) — chunk clipping etc.
    let top = &volume.cuts[10];
    assert_close(top.elevation_deg, 25.0, 1e-5, "top elangle");
    assert_eq!(top.radials[0].gate_range.gate_count, 300);
    let top_dbzh = top.moments.get(&MomentType::Reflectivity).expect("DBZH");
    assert_close(
        top_dbzh.scaled_value(0, 0).unwrap(),
        34.5,
        1e-4,
        "top v[0,0]",
    );
    assert_close(
        top_dbzh.scaled_value(180, 10).unwrap(),
        27.5,
        1e-4,
        "top v[180,10]",
    );
    assert_eq!(top_dbzh.scaled_value(359, 299), None, "top v[359,299]");
}

#[test]
fn real_bewid_pvol_reads_vlen_string_attrs_and_root_nyquist() {
    let volume = nexrad_io::odim::decode_odim_h5_volume(BEWID).expect("decode bewid");

    assert_eq!(volume.site.id, "BEWID");
    assert_eq!(volume.site.name.as_deref(), Some("Wideumont"));
    assert_close(volume.site.latitude_deg.unwrap(), 49.9143, 1e-4, "lat");
    assert_close(volume.site.longitude_deg.unwrap(), 5.5056, 1e-4, "lon");
    assert_close(volume.site.elevation_m.unwrap(), 592.0, 1e-3, "height");
    // /what date+time are VARIABLE-LENGTH strings in this writer — decoding
    // them exercises the hdf5lite global-heap path on real bytes.
    assert_eq!(
        volume.volume_time,
        Utc.with_ymd_and_hms(2013, 4, 29, 4, 30, 0).unwrap()
    );

    assert_eq!(volume.cuts.len(), 5);
    let cut = &volume.cuts[0];
    assert_close(cut.elevation_deg, 0.3, 1e-5, "elangle");
    assert_eq!(cut.radials.len(), 360);
    let gates = &cut.radials[0].gate_range;
    assert_eq!(
        (gates.first_gate_m, gates.gate_spacing_m, gates.gate_count),
        (0, 250, 960)
    );
    // Root /how NI = 7.98 m/s applies to every sweep.
    for cut in &volume.cuts {
        assert_close(
            cut.radials[0].nyquist_velocity_mps.expect("NI"),
            7.98,
            1e-3,
            "root NI",
        );
    }

    let dbzh = cut.moments.get(&MomentType::Reflectivity).expect("DBZH");
    assert_eq!(dbzh.scaled_value(0, 0), None, "v[0,0] undetect");
    assert_close(
        dbzh.scaled_value(180, 10).unwrap(),
        -22.0,
        1e-4,
        "v[180,10]",
    );
    let top = volume.cuts[4]
        .moments
        .get(&MomentType::Reflectivity)
        .expect("DBZH");
    assert_close(
        top.scaled_value(180, 10).unwrap(),
        -20.5,
        1e-4,
        "top v[180,10]",
    );
}

#[test]
fn real_norst_pvol_reads_superblock_v1_and_half_degree_sweep() {
    let volume = nexrad_io::odim::decode_odim_h5_volume(NORST).expect("decode norst");

    // source = "WMO:01104,NOD:norst" — NOD preferred over the leading WMO.
    assert_eq!(volume.site.id, "NORST");
    assert_close(volume.site.latitude_deg.unwrap(), 67.5307, 1e-4, "lat");
    assert_close(volume.site.longitude_deg.unwrap(), 12.0986, 1e-4, "lon");
    assert_eq!(
        volume.volume_time,
        Utc.with_ymd_and_hms(2017, 4, 21, 9, 8, 37).unwrap()
    );

    assert_eq!(volume.cuts.len(), 6);
    assert_eq!(volume.metadata.decoded_radial_count, 2520);

    // Lowest sweep is 720 half-degree rays; centers at 0.25, 0.75, ...
    let cut = &volume.cuts[0];
    assert_close(cut.elevation_deg, 0.5, 1e-5, "elangle");
    assert_eq!(cut.radials.len(), 720);
    assert_close(cut.radials[0].azimuth_deg, 0.25, 1e-5, "az0");
    assert_close(cut.radials[1].azimuth_deg, 0.75, 1e-5, "az1");
    let gates = &cut.radials[0].gate_range;
    assert_eq!(
        (gates.first_gate_m, gates.gate_spacing_m, gates.gate_count),
        (0, 250, 960)
    );

    let dbzh = cut.moments.get(&MomentType::Reflectivity).expect("DBZH");
    assert_eq!(dbzh.scaled_value(0, 0), None, "v[0,0] undetect");
    assert_close(
        dbzh.scaled_value(180, 320).unwrap(),
        2.5,
        1e-4,
        "v[180,320]",
    );
    assert_close(dbzh.scaled_value(360, 10).unwrap(), -7.0, 1e-4, "v[360,10]");
    assert_eq!(dbzh.scaled_value(719, 959), None, "v[719,959] undetect");

    // Upper sweeps drop back to 360 rays and shorter ranges.
    let top = &volume.cuts[5];
    assert_close(top.elevation_deg, 9.4, 1e-5, "top elangle");
    assert_eq!(top.radials.len(), 360);
    assert_eq!(top.radials[0].gate_range.gate_count, 300);
    let top_dbzh = top.moments.get(&MomentType::Reflectivity).expect("DBZH");
    assert_close(
        top_dbzh.scaled_value(180, 10).unwrap(),
        -18.5,
        1e-4,
        "top v[180,10]",
    );
}
