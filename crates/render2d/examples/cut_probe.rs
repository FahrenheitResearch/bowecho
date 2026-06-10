// Print each cut's elevation, Nyquist, and near-Nyquist fraction (aliasing
// pressure) — for understanding cascade-dealias behavior on real volumes.
use radar_core::{MomentType, RadarVolume};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os().nth(1).ok_or("usage: cut_probe <l2>")?;
    let volume: RadarVolume = nexrad_io::decode_volume_from_path(path.as_ref())?;
    for (i, cut) in volume.cuts.iter().enumerate() {
        let Some(grid) = cut.moments.get(&MomentType::Velocity) else {
            continue;
        };
        let nyq = cut
            .radials
            .first()
            .and_then(|r| r.nyquist_velocity_mps)
            .unwrap_or(f32::NAN);
        let mut near = 0u32;
        let mut total = 0u32;
        for row in (0..grid.radial_count()).step_by(4) {
            for gate in (0..grid.gate_range.gate_count).step_by(4) {
                if let Some(v) = grid.scaled_value(row, gate).filter(|v| v.is_finite()) {
                    total += 1;
                    if v.abs() > 0.85 * nyq {
                        near += 1;
                    }
                }
            }
        }
        let pct = if total > 0 {
            100.0 * near as f32 / total as f32
        } else {
            0.0
        };
        println!(
            "#{i:02} {:5.2} deg  nyq {nyq:5.1} m/s  near-Nyquist {pct:4.1}%  ({total} samples)",
            cut.elevation_deg
        );
    }
    Ok(())
}
