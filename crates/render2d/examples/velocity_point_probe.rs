// Probe raw vs dealiased velocity at given az/range points on the lowest
// velocity tilt — for debugging fold failures reported in the field.
// usage: velocity_point_probe <l2-file> <az_deg> <range_km> [<az> <range> ...]
use radar_core::{MomentType, RadarVolume};
use render2d::{dealias_velocity_grid, dealias_velocity_grid_cascade};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("usage: <l2> <az> <rng_km> ...")?;
    let points: Vec<f64> = args.filter_map(|a| a.parse().ok()).collect();
    let volume: RadarVolume =
        nexrad_io::decode_volume_from_path(path.as_ref() as &std::path::Path)?;
    let (index, cut) = volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, c)| c.moments.contains_key(&MomentType::Velocity))
        .min_by(|a, b| a.1.elevation_deg.total_cmp(&b.1.elevation_deg))
        .ok_or("no velocity")?;
    let velocity = cut.moments.get(&MomentType::Velocity).unwrap();
    let dealiased = if std::env::var_os("BOWECHO_CASCADE").is_some() {
        eprintln!("(cascade engine)");
        dealias_velocity_grid_cascade(&volume, index).expect("cascade")
    } else {
        dealias_velocity_grid(cut, velocity)
    };
    println!(
        "cut #{index} elev {:.2} gates {} spacing {} m first {} m",
        cut.elevation_deg,
        velocity.gate_range.gate_count,
        velocity.gate_range.gate_spacing_m,
        velocity.gate_range.first_gate_m
    );
    for pair in points.chunks(2) {
        let [az, range_km] = pair else { continue };
        // nearest radial by azimuth
        let mut best = (usize::MAX, f32::INFINITY);
        for (row, &radial_index) in velocity.radial_indices.iter().enumerate() {
            if let Some(radial) = cut.radials.get(radial_index) {
                let diff = (radial.azimuth_deg - *az as f32).rem_euclid(360.0);
                let diff = diff.min(360.0 - diff);
                if diff < best.1 {
                    best = (row, diff);
                }
            }
        }
        let row = best.0;
        let nyquist = velocity
            .radial_indices
            .get(row)
            .and_then(|&i| cut.radials.get(i))
            .and_then(|r| r.nyquist_velocity_mps);
        let gate = ((range_km * 1000.0 - velocity.gate_range.first_gate_m as f64)
            / velocity.gate_range.gate_spacing_m.max(1) as f64)
            .round() as usize;
        // sample a 5-gate window around the point
        println!("az {az:.1} rng {range_km:.1} km (row {row}, gate {gate}, nyq {nyquist:?}):");
        for g in gate.saturating_sub(2)..=(gate + 2).min(velocity.gate_range.gate_count - 1) {
            let raw = velocity.scaled_value(row, g);
            let dl = dealiased.scaled_value(row, g);
            println!("  gate {g}: raw {raw:?} -> dealiased {dl:?}");
        }
    }
    Ok(())
}
