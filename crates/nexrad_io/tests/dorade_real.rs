//! Golden-fixture tests for the DORADE decoder against real radar bytes.
//!
//! `tests/data/swp.1260521225514.COW2.229.1.0_SUR_v215.head24` is the first
//! 37,380 bytes (all descriptor blocks + the first 24 ray groups, cut at a
//! block boundary) of a real CSWR COW2 sweepfile from the 2026-05-21
//! deployment (Radx-written, big-endian, HRD RLE compressed, CSFD gate
//! geometry, staggered PRT). Expected values below were extracted with an
//! independent Python block walker + RLE decoder, not with this crate.

use chrono::{TimeZone, Utc};
use nexrad_io::dorade::{decode_dorade_sweep_volume, looks_like_dorade_bytes, peek_dorade_sweep};
use radar_core::MomentType;

const FIXTURE: &[u8] = include_bytes!("data/swp.1260521225514.COW2.229.1.0_SUR_v215.head24");

fn assert_close(actual: f32, expected: f32, tolerance: f32, what: &str) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{what}: {actual} != {expected} (tolerance {tolerance})"
    );
}

#[test]
fn real_cow2_sweep_decodes_site_and_geometry() {
    assert!(looks_like_dorade_bytes(FIXTURE));
    let volume = decode_dorade_sweep_volume(FIXTURE).expect("decode COW2 fixture");

    // Site identity and deployment coordinates come from RADD.
    assert_eq!(volume.site.id, "COW2");
    assert_close(volume.site.latitude_deg.unwrap(), 39.74, 1e-4, "latitude");
    assert_close(
        volume.site.longitude_deg.unwrap(),
        -103.2927,
        1e-4,
        "longitude",
    );
    assert_close(volume.site.elevation_m.unwrap(), 1519.0, 0.5, "altitude");

    // Volume time from SSWB.
    assert_eq!(
        volume.volume_time,
        Utc.with_ymd_and_hms(2026, 5, 21, 22, 55, 14).unwrap()
    );
    assert_eq!(
        volume.metadata.compression.as_deref(),
        Some("dorade-hrd-rle")
    );

    // One cut; the fixture's 24 rays include 3 antenna-transition rays that
    // must be dropped.
    assert_eq!(volume.cuts.len(), 1);
    let cut = &volume.cuts[0];
    assert_close(cut.elevation_deg, 1.005_255_6, 1e-5, "fixed angle");
    assert_eq!(cut.radials.len(), 21);
    assert_eq!(volume.metadata.skipped_message_count, 3);

    // CSFD gate geometry: 375 gates, first at 50 m, 100 m spacing.
    let radial = &cut.radials[0];
    assert_eq!(radial.gate_range.gate_count, 375);
    assert_eq!(radial.gate_range.first_gate_m, 50);
    assert_eq!(radial.gate_range.gate_spacing_m, 100);

    // First kept ray: az 73.0, el 0.8184814453125, 22:55:14.280 (offset
    // 280 ms from the SSWB start).
    assert_close(radial.azimuth_deg, 73.0, 1e-4, "azimuth");
    assert_close(radial.elevation_deg, 0.818_481_4, 1e-5, "ray elevation");
    assert_eq!(radial.time_offset_ms, 280);

    // RADD eff_unamb_vel (staggered-PRT extended Nyquist) on every radial.
    assert_close(radial.nyquist_velocity_mps.unwrap(), 68.76, 0.01, "nyquist");
    assert!(
        cut.radials
            .iter()
            .all(|radial| radial.nyquist_velocity_mps.is_some())
    );
}

#[test]
fn real_cow2_sweep_decodes_known_moment_values() {
    let volume = decode_dorade_sweep_volume(FIXTURE).expect("decode COW2 fixture");
    let cut = &volume.cuts[0];

    for moment in [
        MomentType::Reflectivity,
        MomentType::Velocity,
        MomentType::DifferentialReflectivity,
        MomentType::CorrelationCoefficient,
    ] {
        let grid = cut.moments.get(&moment).unwrap_or_else(|| {
            panic!("missing {moment}");
        });
        assert_eq!(grid.radial_count(), 21, "{moment} rows");
        assert_eq!(grid.gate_range.gate_count, 375, "{moment} gates");
    }

    // Row 0 = first kept ray (file ray index 3). Raw i16 values from the
    // independent decoder: REF (scale 100) -3030, bad, -69; VEL (scale 100)
    // -586, 478, 452, ..., 5805; ZDR (scale 100) -189, bad, 221; RHOHV
    // (scale 10000) 3235, bad, 9759.
    let reflectivity = &cut.moments[&MomentType::Reflectivity];
    assert_close(
        reflectivity.scaled_value(0, 0).unwrap(),
        -30.30,
        1e-3,
        "REF gate 0",
    );
    assert_eq!(reflectivity.scaled_value(0, 50), None, "REF gate 50 is bad");
    assert_close(
        reflectivity.scaled_value(0, 100).unwrap(),
        -0.69,
        1e-3,
        "REF gate 100",
    );

    let velocity = &cut.moments[&MomentType::Velocity];
    assert_close(velocity.scaled_value(0, 0).unwrap(), -5.86, 1e-3, "VEL 0");
    assert_close(velocity.scaled_value(0, 50).unwrap(), 4.78, 1e-3, "VEL 50");
    assert_close(
        velocity.scaled_value(0, 100).unwrap(),
        4.52,
        1e-3,
        "VEL 100",
    );
    assert_close(
        velocity.scaled_value(0, 374).unwrap(),
        58.05,
        1e-3,
        "VEL 374 (last gate)",
    );

    let zdr = &cut.moments[&MomentType::DifferentialReflectivity];
    assert_close(zdr.scaled_value(0, 0).unwrap(), -1.89, 1e-3, "ZDR 0");
    assert_close(zdr.scaled_value(0, 100).unwrap(), 2.21, 1e-3, "ZDR 100");

    let rhohv = &cut.moments[&MomentType::CorrelationCoefficient];
    assert_close(rhohv.scaled_value(0, 0).unwrap(), 0.3235, 1e-4, "RHO 0");
    assert_close(rhohv.scaled_value(0, 100).unwrap(), 0.9759, 1e-4, "RHO 100");
}

#[test]
fn real_cow2_sweep_header_peek_matches_full_decode() {
    let header = peek_dorade_sweep(FIXTURE).expect("peek COW2 fixture");
    assert_eq!(header.instrument, "COW2");
    assert_eq!(header.volume_number, 215);
    assert_eq!(header.sweep_number, 6);
    assert_close(header.fixed_angle_deg, 1.005_255_6, 1e-5, "fixed angle");
    assert_eq!(
        header.start_time,
        Some(Utc.with_ymd_and_hms(2026, 5, 21, 22, 55, 14).unwrap())
    );
}

/// Whole-corpus regression: decode every deployment zip and loose sweepfile
/// under the local mobile-radar corpus (DOW7 Goshen 2009, RaXPol Sulphur
/// 2016, COW2/DOW7low Goodland + COW2 deployment 2026, GR2 msg31 twins).
#[test]
#[ignore = "requires BOWECHO_MOBILE_RADAR_DIR or C:\\Users\\drew\\Downloads\\obscure_radar"]
fn mobile_radar_corpus_decodes_every_archive() {
    let corpus = std::env::var_os("BOWECHO_MOBILE_RADAR_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Users\drew\Downloads\obscure_radar"));
    if !corpus.is_dir() {
        eprintln!("skipping corpus test; {} not found", corpus.display());
        return;
    }

    let mut archives = 0usize;
    let mut volumes = 0usize;
    for entry in std::fs::read_dir(&corpus)
        .expect("read corpus dir")
        .flatten()
    {
        let path = entry.path();
        if !nexrad_io::mobile_archive::looks_like_zip_path(&path) {
            continue;
        }
        archives += 1;
        let decoded = nexrad_io::mobile_archive::decode_mobile_archive_from_path(&path)
            .unwrap_or_else(|err| panic!("decode {}: {err}", path.display()));
        assert!(!decoded.is_empty(), "{} has no volumes", path.display());
        for entry in &decoded {
            let volume = &entry.volume;
            assert!(!volume.site.id.is_empty());
            assert!(
                !volume.cuts.is_empty(),
                "{} empty volume",
                entry.member_label
            );
            assert!(
                volume.site.latitude_deg.is_some() && volume.site.longitude_deg.is_some(),
                "{} missing site coords",
                entry.member_label
            );
            for cut in &volume.cuts {
                assert!(!cut.radials.is_empty());
                assert!(!cut.moments.is_empty());
                for grid in cut.moments.values() {
                    assert_eq!(grid.radial_count(), cut.radials.len());
                }
            }
        }
        volumes += decoded.len();
        eprintln!("{}: {} volumes", path.display(), decoded.len());
    }
    assert!(archives > 0, "no zip archives found in corpus");
    eprintln!("corpus total: {archives} archives, {volumes} volumes");
}
