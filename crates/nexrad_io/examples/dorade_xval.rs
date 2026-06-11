//! Cross-validate a DORADE sweepfile against its GR2 MSG31 (Archive II)
//! twin: same scan exported by the radar in both formats.
//!
//! Usage: cargo run -p nexrad_io --example dorade_xval -- <swp.file> <file.msg31>

use radar_core::MomentType;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let (Some(dorade_path), Some(msg31_path)) = (args.next(), args.next()) else {
        eprintln!("usage: dorade_xval <swp.file> <file.msg31>");
        std::process::exit(2);
    };

    let dorade_bytes = std::fs::read(&dorade_path)?;
    let dorade = nexrad_io::dorade::decode_dorade_sweep_volume(&dorade_bytes)?;
    let msg31 = nexrad_io::decode_volume_from_path(msg31_path.as_ref())?;

    println!(
        "DORADE site {} ({:?}, {:?}) t={}",
        dorade.site.id, dorade.site.latitude_deg, dorade.site.longitude_deg, dorade.volume_time
    );
    println!(
        "MSG31  site {} ({:?}, {:?}) t={}",
        msg31.site.id, msg31.site.latitude_deg, msg31.site.longitude_deg, msg31.volume_time
    );

    let dorade_cut = &dorade.cuts[0];
    let msg31_cut = &msg31.cuts[0];
    println!(
        "DORADE cut: {} radials, elev {:.3}; MSG31 cut: {} radials, elev {:.3} (of {} cuts)",
        dorade_cut.radials.len(),
        dorade_cut.elevation_deg,
        msg31_cut.radials.len(),
        msg31_cut.elevation_deg,
        msg31.cuts.len()
    );

    for moment in [
        MomentType::Reflectivity,
        MomentType::Velocity,
        MomentType::DifferentialReflectivity,
        MomentType::CorrelationCoefficient,
    ] {
        let (Some(left), Some(right)) = (
            dorade_cut.moments.get(&moment),
            msg31_cut.moments.get(&moment),
        ) else {
            println!("{moment}: missing on one side");
            continue;
        };
        // Align radials by azimuth: find the MSG31 radial closest to each
        // DORADE radial and compare gates over the overlapping range.
        let mut compared = 0usize;
        let mut both_finite = 0usize;
        let mut max_abs_diff = 0.0f32;
        let mut sum_abs_diff = 0.0f64;
        let mut one_sided = 0usize;
        for (dorade_row, dorade_radial) in dorade_cut.radials.iter().enumerate() {
            let target_azimuth = dorade_radial.azimuth_deg;
            let Some((msg31_row, _)) = msg31_cut
                .radials
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| {
                    azimuth_distance(a.azimuth_deg, target_azimuth)
                        .total_cmp(&azimuth_distance(b.azimuth_deg, target_azimuth))
                })
                .filter(|(_, radial)| azimuth_distance(radial.azimuth_deg, target_azimuth) < 0.25)
            else {
                continue;
            };
            // MSG31 rows are indexed by position in its grid row list.
            let Some(left_grid_row) = left.radial_indices.iter().position(|r| *r == dorade_row)
            else {
                continue;
            };
            let Some(right_grid_row) = right.radial_indices.iter().position(|r| *r == msg31_row)
            else {
                continue;
            };
            let gates = left.gate_range.gate_count.min(right.gate_range.gate_count);
            // Compare at matching ranges (gate spacing may differ).
            for gate in 0..gates {
                let range_m =
                    left.gate_range.first_gate_m + gate as i32 * left.gate_range.gate_spacing_m;
                let right_gate =
                    (range_m - right.gate_range.first_gate_m) / right.gate_range.gate_spacing_m;
                if right_gate < 0 || right_gate as usize >= right.gate_range.gate_count {
                    continue;
                }
                let left_value = left.scaled_value(left_grid_row, gate);
                let right_value = right.scaled_value(right_grid_row, right_gate as usize);
                compared += 1;
                match (left_value, right_value) {
                    (Some(a), Some(b)) => {
                        both_finite += 1;
                        let diff = (a - b).abs();
                        max_abs_diff = max_abs_diff.max(diff);
                        sum_abs_diff += f64::from(diff);
                    }
                    (None, None) => {}
                    _ => one_sided += 1,
                }
            }
        }
        println!(
            "{moment}: {compared} gates compared, {both_finite} both-finite, mean |diff| {:.4}, max |diff| {:.4}, {one_sided} one-sided",
            if both_finite > 0 {
                sum_abs_diff / both_finite as f64
            } else {
                f64::NAN
            },
            max_abs_diff
        );
    }
    Ok(())
}

fn azimuth_distance(a: f32, b: f32) -> f32 {
    let diff = (a - b).rem_euclid(360.0);
    diff.min(360.0 - diff)
}
