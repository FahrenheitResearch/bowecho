// Validate TEMPORAL-reference dealiasing: dealias volume A (plain region
// engine), fit the range-band reference from its lowest tilt, then dealias
// volume B's lowest tilt WITH that reference. Reports B's largest positive
// (outbound) clusters — the fold-branch failure mode.
// usage: dealias_temporal_probe <volume_A> <volume_B>
use radar_core::{MomentType, RadarVolume};
use render2d::{
    dealias_velocity_grid, dealias_velocity_grid_with_reference, fit_range_band_reference,
};

fn lowest_velocity_cut(volume: &RadarVolume) -> Option<usize> {
    volume
        .cuts
        .iter()
        .enumerate()
        .filter(|(_, c)| c.moments.contains_key(&MomentType::Velocity))
        .min_by(|a, b| a.1.elevation_deg.total_cmp(&b.1.elevation_deg))
        .map(|(i, _)| i)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path_a = args.next().ok_or("usage: <volA> <volB>")?;
    let path_b = args.next().ok_or("usage: <volA> <volB>")?;

    let volume_a: RadarVolume =
        nexrad_io::decode_volume_from_path(path_a.as_ref() as &std::path::Path)?;
    let cut_a = lowest_velocity_cut(&volume_a).ok_or("A: no velocity")?;
    let grid_a = volume_a.cuts[cut_a]
        .moments
        .get(&MomentType::Velocity)
        .unwrap();
    let dealiased_a = dealias_velocity_grid(&volume_a.cuts[cut_a], grid_a);
    let reference = fit_range_band_reference(&volume_a.cuts[cut_a], &dealiased_a);
    let valid = reference.fits.iter().filter(|f| f.is_some()).count();
    println!(
        "reference from {}: {}/{} bands",
        path_a,
        valid,
        reference.fits.len()
    );

    let volume_b: RadarVolume =
        nexrad_io::decode_volume_from_path(path_b.as_ref() as &std::path::Path)?;
    let cut_b = lowest_velocity_cut(&volume_b).ok_or("B: no velocity")?;
    let cut = &volume_b.cuts[cut_b];
    let grid_b = cut.moments.get(&MomentType::Velocity).unwrap();
    let dealiased = dealias_velocity_grid_with_reference(cut, grid_b, Some(&reference));

    // Largest positive clusters within 80 km (the failure signature).
    let rows = dealiased.radial_count();
    let gates = dealiased.gate_range.gate_count;
    let spacing = dealiased.gate_range.gate_spacing_m.max(1) as f64;
    let first = dealiased.gate_range.first_gate_m as f64;
    let max_gate = (((80_000.0 - first) / spacing) as usize).min(gates);
    let mut flagged = vec![false; rows * gates];
    for row in 0..rows {
        for gate in 0..max_gate {
            if let Some(v) = dealiased.scaled_value(row, gate).filter(|v| v.is_finite())
                && v > 10.0
            {
                flagged[row * gates + gate] = true;
            }
        }
    }
    let mut visited = vec![false; rows * gates];
    let mut clusters: Vec<(usize, usize)> = Vec::new();
    let mut stack = Vec::new();
    for seed in 0..rows * gates {
        if !flagged[seed] || visited[seed] {
            continue;
        }
        stack.clear();
        stack.push(seed);
        visited[seed] = true;
        let mut size = 0usize;
        let mut peak = f32::NEG_INFINITY;
        let mut peak_cell = seed;
        while let Some(cell) = stack.pop() {
            size += 1;
            let (r, g) = (cell / gates, cell % gates);
            if let Some(v) = dealiased.scaled_value(r, g)
                && v > peak
            {
                peak = v;
                peak_cell = cell;
            }
            for (dr, dg) in [(1i64, 0i64), (-1, 0), (0, 1), (0, -1)] {
                let rr = ((r as i64 + dr).rem_euclid(rows as i64)) as usize;
                let gg = g as i64 + dg;
                if gg < 0 || gg >= gates as i64 {
                    continue;
                }
                let idx = rr * gates + gg as usize;
                if flagged[idx] && !visited[idx] {
                    visited[idx] = true;
                    stack.push(idx);
                }
            }
        }
        clusters.push((size, peak_cell));
    }
    clusters.sort_by(|a, b| b.0.cmp(&a.0));
    println!("B largest positive clusters (within 80 km):");
    for (size, peak_cell) in clusters.iter().take(5) {
        let (row, gate) = (peak_cell / gates, peak_cell % gates);
        let az = dealiased
            .radial_indices
            .get(row)
            .and_then(|&i| cut.radials.get(i))
            .map(|r| r.azimuth_deg)
            .unwrap_or(0.0);
        let raw = grid_b.scaled_value(row, gate);
        let dl = dealiased.scaled_value(row, gate);
        println!(
            "  {size} gates @ az {az:.1} rng {:.1} km: raw {raw:?} -> dealiased {dl:?}",
            (first + gate as f64 * spacing) / 1000.0
        );
    }
    Ok(())
}
