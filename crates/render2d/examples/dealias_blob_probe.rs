// Hunt dealiasing failures: find large clusters where the DEALIASED velocity
// is strongly positive (outbound) and report their raw values — a cluster
// whose dealiased = raw + 2·Nyq with negative surroundings is an over-unfold;
// raw==dealiased positive amid negatives is a missed unfold.
// usage: dealias_blob_probe <l2-file>
use radar_core::{MomentType, RadarVolume};
use render2d::{dealias_velocity_grid, dealias_velocity_grid_cascade};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: <l2>")?;
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
    let rows = dealiased.radial_count();
    let gates = dealiased.gate_range.gate_count;
    let spacing = dealiased.gate_range.gate_spacing_m.max(1) as f64;
    let first = dealiased.gate_range.first_gate_m as f64;
    println!("cut #{index} elev {:.2}", cut.elevation_deg);

    // Cluster strongly-positive dealiased gates within 80 km.
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
    let mut clusters: Vec<(usize, usize, usize)> = Vec::new(); // (size, peak_cell, cluster_id)
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
        clusters.push((size, peak_cell, clusters.len()));
    }
    clusters.sort_by(|a, b| b.0.cmp(&a.0));
    for (size, peak_cell, _) in clusters.iter().take(5) {
        let (row, gate) = (peak_cell / gates, peak_cell % gates);
        let az = dealiased
            .radial_indices
            .get(row)
            .and_then(|&i| cut.radials.get(i))
            .map(|r| r.azimuth_deg)
            .unwrap_or(0.0);
        let nyq = dealiased
            .radial_indices
            .get(row)
            .and_then(|&i| cut.radials.get(i))
            .and_then(|r| r.nyquist_velocity_mps);
        let range_km = (first + gate as f64 * spacing) / 1000.0;
        let raw = velocity.scaled_value(row, gate);
        let dl = dealiased.scaled_value(row, gate);
        println!(
            "cluster {size} gates @ az {az:.1} rng {range_km:.1} km: raw {raw:?} -> dealiased {dl:?} (nyq {nyq:?})"
        );
        // neighbors along the radial for context
        for g in gate.saturating_sub(6)..=(gate + 6).min(gates - 1) {
            if g % 2 == 0 {
                let r2 = velocity.scaled_value(row, g);
                let d2 = dealiased.scaled_value(row, g);
                println!("   g{g}: raw {r2:?} dl {d2:?}");
            }
        }
    }
    Ok(())
}
