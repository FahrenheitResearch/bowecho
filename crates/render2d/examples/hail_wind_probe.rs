//! Probe the hail + wind products on a real volume (KEAX derecho).
//! Usage: hail_wind_probe <archive file> [h0_km] [hm20_km]

use render2d::{MeshCalibration, hail_grids, poh_grid};
use render2d::{gust_proxy_grid, marc_grid};

fn stats(name: &str, grid: &radar_core::MomentGrid, thresholds: &[f32]) {
    let rows = grid.radial_count();
    let gates = grid.gate_range.gate_count;
    let mut values: Vec<f32> = Vec::new();
    for row in 0..rows {
        for gate in 0..gates {
            if let Some(v) = grid.scaled_value(row, gate)
                && v.is_finite()
            {
                values.push(v);
            }
        }
    }
    if values.is_empty() {
        println!("{name}: EMPTY");
        return;
    }
    values.sort_by(f32::total_cmp);
    let max = values[values.len() - 1];
    let p99 = values[((values.len() as f64 * 0.99) as usize).min(values.len() - 1)];
    print!("{name}: n={} max={max:.1} p99={p99:.1}", values.len());
    for &t in thresholds {
        let count = values.iter().filter(|v| **v >= t).count();
        print!(" ≥{t}:{count}");
    }
    println!();
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: hail_wind_probe <file> [h0_km] [hm20_km]");
    let h0_km: f32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(3.2);
    let hm20_km: f32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(6.2);
    let volume = nexrad_io::decode_volume_from_path(std::path::Path::new(&path)).expect("decode");
    println!(
        "{} cuts, H0={h0_km} km, H-20={hm20_km} km",
        volume.cuts.len()
    );

    for cal in [
        MeshCalibration::Witt1998,
        MeshCalibration::MurilloHomeyer2019P75,
        MeshCalibration::MurilloHomeyer2019P95,
    ] {
        if let Some(hail) = hail_grids(&volume, h0_km * 1000.0, hm20_km * 1000.0, cal) {
            stats(&format!("MESH {cal:?}"), &hail.mesh_mm, &[19.0, 29.0, 47.0]);
            if cal == MeshCalibration::Witt1998 {
                stats("SHI", &hail.shi, &[100.0]);
                stats("POSH", &hail.posh_pct, &[50.0, 70.0]);
            }
        }
    }
    if let Some(poh) = poh_grid(&volume, h0_km * 1000.0) {
        stats("POH", &poh, &[50.0, 80.0]);
    }
    if let Some(marc) = marc_grid(&volume) {
        stats("MARC dV", &marc, &[25.0, 30.0, 38.0]);
    }
    if let Some(gust) = gust_proxy_grid(&volume) {
        stats("Gust proxy", &gust, &[25.0, 32.0]);
    }
}
