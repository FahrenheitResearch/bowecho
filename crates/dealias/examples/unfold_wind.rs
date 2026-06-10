//! Synthesize an aliased uniform-wind sweep, dealias it, and report residual
//! fold boundaries — a quick smoke test of the engine.
//!
//! Run: `cargo run -p bowecho-dealias --example unfold_wind`

use bowecho_dealias::{Sweep, dealias};

fn main() {
    let rows = 720usize; // 0.5° super-resolution sweep
    let gates = 400usize;
    let nyquist = 22.0f32;
    let (speed, toward_deg) = (38.0f32, 240.0f32);

    // True field: uniform wind, radial component speed·cos(az − dir),
    // wrapped into ±Nyquist exactly as the radar would observe it.
    let azimuths: Vec<f32> = (0..rows).map(|r| r as f32 * 360.0 / rows as f32).collect();
    let mut observed = vec![f32::NAN; rows * gates];
    let mut truth = vec![f32::NAN; rows * gates];
    for row in 0..rows {
        let v = speed * (azimuths[row] - toward_deg).to_radians().cos();
        let wrapped = v - (v / (2.0 * nyquist)).round() * 2.0 * nyquist;
        for gate in 0..gates {
            truth[row * gates + gate] = v;
            observed[row * gates + gate] = wrapped;
        }
    }

    let nyq = vec![nyquist; rows];
    let result = dealias(&Sweep {
        velocity: &observed,
        gates,
        nyquist: &nyq,
        azimuths_deg: &azimuths,
    });

    let boundaries = |field: &[f32]| {
        let mut count = 0usize;
        for row in 0..rows {
            for gate in 0..gates - 1 {
                let (a, b) = (field[row * gates + gate], field[row * gates + gate + 1]);
                if a.is_finite() && b.is_finite() && (a - b).abs() > nyquist {
                    count += 1;
                }
            }
        }
        for gate in 0..gates {
            for row in 0..rows {
                let down = (row + 1) % rows;
                let (a, b) = (field[row * gates + gate], field[down * gates + gate]);
                if a.is_finite() && b.is_finite() && (a - b).abs() > nyquist {
                    count += 1;
                }
            }
        }
        count
    };

    let worst = result
        .velocity
        .iter()
        .zip(truth.iter())
        .filter(|(v, _)| v.is_finite())
        .map(|(v, t)| (v - t).abs())
        .fold(0.0f32, f32::max)
        // The region engine anchors per group; a uniform wind has two big
        // groups (inbound/outbound), so allow one global 2·Nyquist branch.
        .min(
            result
                .velocity
                .iter()
                .zip(truth.iter())
                .filter(|(v, _)| v.is_finite())
                .map(|(v, t)| ((v - t).abs() - 2.0 * nyquist).abs())
                .fold(0.0f32, f32::max),
        );

    println!("{speed} m/s wind, Nyquist {nyquist} m/s, {rows}x{gates} gates");
    println!("fold boundaries  raw: {:6}", boundaries(&observed));
    println!("fold boundaries  out: {:6}", boundaries(&result.velocity));
    println!("worst error vs truth (mod one branch): {worst:.2} m/s");
}
