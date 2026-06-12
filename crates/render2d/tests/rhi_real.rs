//! RHI panel math validated against a REAL DOW8 native RHI sweep.
//!
//! The unit tests in `src/rhi.rs` use synthetic beam fans; this exercises
//! the same entry points the app's RHI panel calls (`cut_looks_like_rhi`,
//! `rhi_fixed_azimuth_deg`, `rhi_coverage_*`, `rhi_section`) on the FARM
//! DOW8 truck RHI fixture decoded through the real CfRadial path. See
//! `nexrad_io/tests/cfradial_real_files.rs` for fixture provenance
//! (open-radar-data, MIT; netCDF-4 -> classic container conversion,
//! 3 of 8 fields kept).

use radar_core::{MomentType, ScanMode};

const DOW8_RHI: &[u8] =
    include_bytes!("../../nexrad_io/tests/data/cfrad.20211011_223602_DOW8_RHI.trim3.nc");

#[test]
fn real_dow8_rhi_drives_the_rhi_panel_pipeline() {
    let volume = nexrad_io::cfradial::decode_cfradial1_volume(DOW8_RHI).expect("decode DOW8 RHI");
    // The app's panel gate: declared scan mode wins (volume_is_rhi).
    assert_eq!(volume.metadata.scan_mode, Some(ScanMode::Rhi));
    let cut = &volume.cuts[0];

    // The geometric fallback must also recognize the real sweep.
    assert!(render2d::cut_looks_like_rhi(cut));
    let azimuth = render2d::rhi_fixed_azimuth_deg(cut);
    assert!(
        (azimuth - 184.08).abs() < 0.1,
        "fixed azimuth was {azimuth}"
    );

    let grid = cut.moments.get(&MomentType::Reflectivity).expect("REF");

    // Coverage extents: 950 gates x 125 m = 118.75 km of slant range, top
    // beam at 70 deg elevation -> ~112 km of height, lowest beam slightly
    // below the horizon -> ground range just under the full slant range.
    let top = render2d::rhi_coverage_top_m(cut, grid);
    let expected_top = radar_core::beam_height_above_radar_m(118_750.0, 70.0) as f32;
    assert!(
        (top - expected_top).abs() < 1.0,
        "coverage top {top} != {expected_top}"
    );
    let range = render2d::rhi_coverage_range_m(cut, grid);
    assert!(
        (110_000.0..=119_000.0).contains(&range),
        "coverage range was {range}"
    );

    // Resample the panel the way the app does (768x320 texture).
    let (width, height) = (768usize, 320usize);
    let (top_m, max_range_m) = (15_000.0f32, 60_000.0f32);
    let section =
        render2d::rhi_section(cut, grid, width, height, top_m, max_range_m).expect("section");
    assert_eq!(section.values.len(), width * height);

    // Pixel-exact spot check: take a real echo on a known beam/gate, project
    // it through the forward 4/3-Earth model, and require the panel pixel to
    // hold a value from that beam within the pixel's gate quantization
    // (+/- 2 gates: 60 km / 768 px = 78 m/px vs 125 m gates).
    let beam = 37usize;
    let gate = 316usize;
    let elevation = f64::from(cut.radials[beam].elevation_deg);
    let slant_m = f64::from(grid.gate_range.first_gate_m)
        + f64::from(grid.gate_range.gate_spacing_m) * gate as f64;
    let expected = grid.scaled_value(beam, gate).expect("echo at [37,316]");
    assert!(
        (expected - 0.79).abs() < 1e-3,
        "fixture drifted: {expected}"
    );
    let z = radar_core::beam_height_above_radar_m(slant_m, elevation) as f32;
    let s = radar_core::beam_ground_range_m(slant_m, elevation) as f32;
    assert!(z < top_m && s < max_range_m, "sample outside panel");
    let x = (s / max_range_m * (width - 1) as f32).round() as usize;
    let y = ((1.0 - z / top_m) * (height - 1) as f32).round() as usize;
    let sampled = section.values[y * width + x];
    let candidates: Vec<f32> = (gate.saturating_sub(2)..=gate + 2)
        .filter_map(|g| grid.scaled_value(beam, g))
        .collect();
    assert!(
        candidates.contains(&sampled),
        "panel pixel {sampled} not among beam-37 gates {gate}±2: {candidates:?}"
    );

    // Above the 70-degree top beam (plus the 1-degree beam gap) the panel
    // must stay empty: 12 km up at 3 km out needs ~76 degrees.
    let x = (3_000.0f32 / max_range_m * (width - 1) as f32).round() as usize;
    let y = ((1.0 - 12_000.0f32 / top_m) * (height - 1) as f32).round() as usize;
    assert!(
        section.values[y * width + x].is_nan(),
        "expected empty wedge above the top beam"
    );

    // The scanned fan should actually fill the panel: most pixels under the
    // 70-degree line and inside gate coverage carry data (NCP-filtered
    // fields leave holes, so demand a sane floor, not 100%).
    let filled = section
        .values
        .iter()
        .filter(|value| value.is_finite())
        .count();
    let fraction = filled as f32 / section.values.len() as f32;
    assert!(
        fraction > 0.5,
        "only {:.1}% of the RHI panel carries data",
        fraction * 100.0
    );
}
