// Objective tracker comparison on a real multi-volume sequence
// (Lakshmanan & Smith 2010, Wea. Forecasting 25(2), 721–729): identical
// per-volume cell streams feed (a) the OLD greedy nearest-to-prediction
// tracker (replicated from the pre-rebuild app logic) and (b) the new
// StormTracker. Metrics: median track duration, mismatch error mean(σ_Z)
// over the longest 50%, linearity error mean(e_xy) over the longest 50%.
// usage: track_quality_probe <l2-file> <l2-file> ...
use chrono::{DateTime, Utc};
use radar_core::RadarVolume;
use render2d::{StormCell, StormTracker, identify_storm_cells};

// ---- replicated OLD tracker (greedy nearest, flat 16 km gate, LSQ<=6,
// null motion on >60 m/s, drop at 2 missed) ----
struct OldTrack {
    history: Vec<(DateTime<Utc>, f64, f64)>,
    dbz: Vec<f32>,
    max_dbz: f32,
    motion: Option<(f64, f64)>,
    missed: u32,
}

fn old_associate(
    tracks: &mut Vec<OldTrack>,
    dropped: &mut Vec<OldTrack>,
    time: DateTime<Utc>,
    cells: &[StormCell],
) {
    const MATCH_LIMIT_KM: f64 = 16.0;
    let mut used = vec![false; cells.len()];
    for track in tracks.iter_mut() {
        let Some(&(t0, e, n)) = track.history.last() else {
            continue;
        };
        let dt = (time - t0).num_milliseconds() as f64 / 1000.0;
        let (ve, vn) = track.motion.unwrap_or((0.0, 0.0));
        let (pe, pn) = (e + ve * dt / 1000.0, n + vn * dt / 1000.0);
        let mut best: Option<(usize, f64)> = None;
        for (j, cell) in cells.iter().enumerate() {
            if used[j] {
                continue;
            }
            let d = ((cell.east_km - pe).powi(2) + (cell.north_km - pn).powi(2)).sqrt();
            if d <= MATCH_LIMIT_KM && best.map(|(_, bd)| d < bd).unwrap_or(true) {
                best = Some((j, d));
            }
        }
        if let Some((j, _)) = best {
            used[j] = true;
            track
                .history
                .push((time, cells[j].east_km, cells[j].north_km));
            track.dbz.push(cells[j].max_dbz);
            track.max_dbz = cells[j].max_dbz;
            track.missed = 0;
            // LSQ over last 6, null on >60 m/s (the old behavior).
            let pts: Vec<(f64, f64, f64)> = track
                .history
                .iter()
                .rev()
                .take(6)
                .map(|&(t, e, n)| (t.timestamp_millis() as f64 / 1000.0, e, n))
                .collect();
            track.motion = if pts.len() >= 2 {
                let t0 = pts[0].0;
                let n_p = pts.len() as f64;
                let (mut st, mut st2, mut se, mut ste, mut sn, mut stn) =
                    (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
                for &(t, e, n) in &pts {
                    let dt = t - t0;
                    st += dt;
                    st2 += dt * dt;
                    se += e;
                    ste += dt * e;
                    sn += n;
                    stn += dt * n;
                }
                let denom = n_p * st2 - st * st;
                if denom.abs() > 1e-6 {
                    let ve = (n_p * ste - st * se) / denom * 1000.0;
                    let vn = (n_p * stn - st * sn) / denom * 1000.0;
                    ((ve * ve + vn * vn).sqrt() <= 60.0).then_some((ve, vn))
                } else {
                    None
                }
            } else {
                None
            };
        } else {
            track.missed += 1;
        }
    }
    let mut keep = Vec::new();
    for track in tracks.drain(..) {
        if track.missed < 2 {
            keep.push(track);
        } else {
            dropped.push(track);
        }
    }
    *tracks = keep;
    for (j, cell) in cells.iter().enumerate() {
        if !used[j] {
            tracks.push(OldTrack {
                history: vec![(time, cell.east_km, cell.north_km)],
                dbz: vec![cell.max_dbz],
                max_dbz: cell.max_dbz,
                motion: None,
                missed: 0,
            });
        }
    }
}

// ---- metrics (Lakshmanan & Smith 2010) ----
struct TrackData {
    fixes: Vec<(f64, f64, f64)>, // (t_s, e, n)
    dbz: Vec<f32>,
}

fn metrics(label: &str, tracks: &[TrackData]) {
    if tracks.is_empty() {
        println!("{label}: no tracks");
        return;
    }
    let mut durations: Vec<f64> = tracks
        .iter()
        .map(|t| t.fixes.last().unwrap().0 - t.fixes.first().unwrap().0)
        .collect();
    durations.sort_by(f64::total_cmp);
    let median_dur = durations[durations.len() / 2];
    // Longest 50% by duration.
    let long: Vec<&TrackData> = tracks
        .iter()
        .filter(|t| {
            t.fixes.last().unwrap().0 - t.fixes.first().unwrap().0 >= median_dur
                && t.fixes.len() >= 3
        })
        .collect();
    let sigma_z: Vec<f64> = long
        .iter()
        .map(|t| {
            let mean = t.dbz.iter().map(|&z| z as f64).sum::<f64>() / t.dbz.len() as f64;
            (t.dbz
                .iter()
                .map(|&z| (z as f64 - mean).powi(2))
                .sum::<f64>()
                / t.dbz.len() as f64)
                .sqrt()
        })
        .collect();
    // Linearity: orthogonal RMSE about the total-least-squares line.
    let exy: Vec<f64> = long
        .iter()
        .map(|t| {
            let n = t.fixes.len() as f64;
            let me = t.fixes.iter().map(|f| f.1).sum::<f64>() / n;
            let mn = t.fixes.iter().map(|f| f.2).sum::<f64>() / n;
            let (mut sxx, mut sxy, mut syy) = (0.0, 0.0, 0.0);
            for f in &t.fixes {
                sxx += (f.1 - me) * (f.1 - me);
                sxy += (f.1 - me) * (f.2 - mn);
                syy += (f.2 - mn) * (f.2 - mn);
            }
            // TLS direction = principal eigenvector of the 2x2 scatter.
            let trace = sxx + syy;
            let det = sxx * syy - sxy * sxy;
            let lam = trace / 2.0 + ((trace / 2.0).powi(2) - det).max(0.0).sqrt();
            let (dx, dy) = if sxy.abs() > 1e-9 {
                (lam - syy, sxy)
            } else if sxx >= syy {
                (1.0, 0.0)
            } else {
                (0.0, 1.0)
            };
            let len = (dx * dx + dy * dy).sqrt().max(1e-9);
            let (ux, uy) = (dx / len, dy / len);
            let mse = t
                .fixes
                .iter()
                .map(|f| {
                    let rx = f.1 - me;
                    let ry = f.2 - mn;
                    let perp = rx * uy - ry * ux;
                    perp * perp
                })
                .sum::<f64>()
                / n;
            mse.sqrt()
        })
        .collect();
    let mean = |v: &[f64]| {
        if v.is_empty() {
            f64::NAN
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };
    let ci = |v: &[f64]| {
        if v.len() < 2 {
            return f64::NAN;
        }
        let m = mean(v);
        let sd = (v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
        0.67 * sd / (v.len() as f64).sqrt()
    };
    println!(
        "{label}: tracks {:3}  median_dur {:5.0} s  mean(sigma_Z) {:5.2} +/-{:4.2} dBZ  mean(e_xy) {:5.2} +/-{:4.2} km   [longest-50% n={}]",
        tracks.len(),
        median_dur,
        mean(&sigma_z),
        ci(&sigma_z),
        mean(&exy),
        ci(&exy),
        long.len()
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.len() < 3 {
        return Err("need >= 3 volumes".into());
    }
    // Identify once; identical streams feed both trackers (the L&S 2010
    // protocol: vary only association).
    let mut streams: Vec<(DateTime<Utc>, Vec<StormCell>)> = Vec::new();
    for path in &paths {
        let volume: RadarVolume =
            nexrad_io::decode_volume_from_path(path.as_ref() as &std::path::Path)?;
        let cells = identify_storm_cells(&volume);
        println!(
            "{}: {} cells",
            path.rsplit(['/', '\\']).next().unwrap_or(path),
            cells.len()
        );
        streams.push((volume.volume_time, cells));
    }
    streams.sort_by_key(|s| s.0);

    // OLD greedy.
    let mut old_tracks: Vec<OldTrack> = Vec::new();
    let mut old_dropped: Vec<OldTrack> = Vec::new();
    for (time, cells) in &streams {
        old_associate(&mut old_tracks, &mut old_dropped, *time, cells);
    }
    let old_done: Vec<TrackData> = old_tracks
        .iter()
        .chain(old_dropped.iter())
        .map(|t| TrackData {
            fixes: t
                .history
                .iter()
                .map(|&(t, e, n)| (t.timestamp_millis() as f64 / 1000.0, e, n))
                .collect(),
            dbz: t.dbz.clone(),
        })
        .collect();

    // NEW tracker (records per-fix dbz via track snapshots after each volume).
    let mut tracker = StormTracker::default();
    let mut dbz_log: std::collections::HashMap<u32, Vec<f32>> = std::collections::HashMap::new();
    let mut final_tracks: std::collections::HashMap<u32, Vec<(f64, f64, f64)>> =
        std::collections::HashMap::new();
    for (time, cells) in &streams {
        tracker.associate(*time, cells, None);
        for track in tracker.tracks.iter().filter(|t| t.merged_into.is_none()) {
            if track.last_fix().map(|(t, ..)| t == *time).unwrap_or(false) {
                dbz_log.entry(track.id).or_default().push(track.max_dbz);
                final_tracks.entry(track.id).or_default().push({
                    let (t, e, n) = track.last_fix().unwrap();
                    (t.timestamp_millis() as f64 / 1000.0, e, n)
                });
            }
        }
    }
    let new_done: Vec<TrackData> = final_tracks
        .into_iter()
        .map(|(id, fixes)| TrackData {
            fixes,
            dbz: dbz_log.remove(&id).unwrap_or_default(),
        })
        .collect();

    println!("\n=== Lakshmanan & Smith 2010 metrics (same cell streams) ===");
    metrics("OLD greedy   ", &old_done);
    metrics("NEW assignment", &new_done);
    Ok(())
}
