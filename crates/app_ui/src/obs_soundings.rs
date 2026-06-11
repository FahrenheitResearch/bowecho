//! Observed (radiosonde) soundings, rendered through the SAME native
//! skew-T pipeline as model soundings — full sharprs parameter suites on
//! real RAOB launches.
//!
//! Source: Iowa Environmental Mesonet's RAOB archive (JSON, no key):
//! site list from the RAOB network GeoJSON, profiles from json/raob.py
//! at the synoptic hours. Archive-aware: callers pass the displayed
//! frame's time and get the launch nearest BEFORE it.
//!
//! (ACARS/AMDAR aircraft profiles are MADIS-gated — the public GSL text
//! server is gone — so aircraft soundings wait on a bring-your-own
//! credentials integration.)

use chrono::{DateTime, Duration, Timelike, Utc};

pub struct RaobSite {
    pub id: String,
    pub lat: f32,
    pub lon: f32,
}

/// Fetch the RAOB site list (cached by the caller).
pub fn fetch_sites() -> Result<Vec<RaobSite>, String> {
    let text =
        data_source::fetch_text("https://mesonet.agron.iastate.edu/geojson/network/RAOB.geojson")
            .map_err(|e| e.to_string())?;
    let root: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let mut sites = Vec::new();
    if let Some(features) = root["features"].as_array() {
        for f in features {
            let sid = f["properties"]["sid"].as_str().unwrap_or("");
            let id = sid.trim_start_matches('_').to_owned();
            let coords = &f["geometry"]["coordinates"];
            if let (Some(lon), Some(lat)) = (coords[0].as_f64(), coords[1].as_f64())
                && !id.is_empty()
            {
                sites.push(RaobSite {
                    id,
                    lat: lat as f32,
                    lon: lon as f32,
                });
            }
        }
    }
    if sites.is_empty() {
        return Err("RAOB site list empty".to_owned());
    }
    Ok(sites)
}

/// The synoptic launch (00/12z) nearest BEFORE `when` (+90 min grace for
/// data arrival), then walk back up to 4 launches if a fetch is empty.
pub fn launch_times_before(when: DateTime<Utc>) -> Vec<DateTime<Utc>> {
    let adjusted = when + Duration::minutes(90);
    let mut t = adjusted
        .date_naive()
        .and_hms_opt(if adjusted.hour() >= 12 { 12 } else { 0 }, 0, 0)
        .unwrap()
        .and_utc();
    if t > adjusted {
        t -= Duration::hours(12);
    }
    (0..4).map(|i| t - Duration::hours(12 * i)).collect()
}

/// Fetch one RAOB profile into the native-sounding column shape.
/// Levels need pres/height/temp/dew; missing winds are interpolated
/// linearly between known levels (edges take the nearest known value)
/// so the hodograph stays honest without zero-wind artifacts.
pub fn fetch_raob(
    station: &str,
    launch: DateTime<Utc>,
) -> Result<rustwx_sounding::SoundingColumn, String> {
    let ts = launch.format("%Y%m%d%H").to_string();
    let url = format!("https://mesonet.agron.iastate.edu/json/raob.py?ts={ts}&station={station}");
    let text = data_source::fetch_text(&url).map_err(|e| e.to_string())?;
    let root: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let profile = root["profiles"]
        .as_array()
        .and_then(|p| p.first())
        .ok_or("no profile")?;
    let levels = profile["profile"].as_array().ok_or("no levels")?;
    let mut pres = Vec::new();
    let mut hght = Vec::new();
    let mut tmpc = Vec::new();
    let mut dwpc = Vec::new();
    let mut wind: Vec<Option<(f64, f64)>> = Vec::new();
    for level in levels {
        let f = |k: &str| level[k].as_f64();
        let (Some(p), Some(h), Some(t), Some(td)) = (f("pres"), f("hght"), f("tmpc"), f("dwpc"))
        else {
            continue;
        };
        // Descending pressure, deduped.
        if pres.last().is_some_and(|&last: &f64| p >= last) {
            continue;
        }
        pres.push(p);
        hght.push(h);
        tmpc.push(t);
        dwpc.push(td.min(t));
        wind.push(match (f("drct"), f("sknt")) {
            (Some(d), Some(s)) => {
                let speed_ms = s * 0.514_444;
                let rad = d.to_radians();
                Some((-speed_ms * rad.sin(), -speed_ms * rad.cos()))
            }
            _ => None,
        });
    }
    if pres.len() < 10 {
        return Err(format!("RAOB {station} {ts}: too few levels"));
    }
    // Interpolate missing winds between known neighbors (index space —
    // RAOB levels are dense enough that this is equivalent to log-p).
    let known: Vec<usize> = (0..wind.len()).filter(|&i| wind[i].is_some()).collect();
    if known.is_empty() {
        return Err("RAOB has no winds".to_owned());
    }
    let (mut u_ms, mut v_ms) = (vec![0.0f64; wind.len()], vec![0.0f64; wind.len()]);
    for i in 0..wind.len() {
        let (u, v) = match wind[i] {
            Some(pair) => pair,
            None => {
                let after = known.iter().copied().find(|&k| k > i);
                let before = known.iter().copied().rev().find(|&k| k < i);
                match (before, after) {
                    (Some(b), Some(a)) => {
                        let t = (i - b) as f64 / (a - b) as f64;
                        let (ub, vb) = wind[b].unwrap();
                        let (ua, va) = wind[a].unwrap();
                        (ub + (ua - ub) * t, vb + (va - vb) * t)
                    }
                    (Some(b), None) => wind[b].unwrap(),
                    (None, Some(a)) => wind[a].unwrap(),
                    (None, None) => (0.0, 0.0),
                }
            }
        };
        u_ms[i] = u;
        v_ms[i] = v;
    }
    let n = pres.len();
    Ok(rustwx_sounding::SoundingColumn {
        pressure_hpa: pres,
        height_m_msl: hght,
        temperature_c: tmpc,
        dewpoint_c: dwpc,
        u_ms,
        v_ms,
        omega_pa_s: vec![0.0; n],
        metadata: rustwx_sounding::SoundingMetadata {
            station_id: station.to_owned(),
            valid_time: launch.format("%Y-%m-%d %Hz").to_string(),
            ..Default::default()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live repro for the field report "sounding compute failed" —
    /// network test, run with --ignored.
    #[test]
    #[ignore]
    fn live_raob_roundtrip() {
        let when = Utc::now();
        for station in ["GRB", "DVN", "ILX", "OAX"] {
            for launch in launch_times_before(when) {
                match fetch_raob(station, launch) {
                    Ok(column) => {
                        println!(
                            "{station} {launch}: {} levels, p {:.0}..{:.0}, h {:.0}..{:.0}",
                            column.pressure_hpa.len(),
                            column.pressure_hpa.first().unwrap(),
                            column.pressure_hpa.last().unwrap(),
                            column.height_m_msl.first().unwrap(),
                            column.height_m_msl.last().unwrap()
                        );
                        match rustwx_sounding::NativeSounding::from_column(&column) {
                            Ok(_) => println!("  from_column OK"),
                            Err(e) => println!("  from_column ERR: {e}"),
                        }
                        return;
                    }
                    Err(e) => println!("{station} {launch}: fetch {e}"),
                }
            }
        }
        panic!("no station produced a column");
    }

    #[test]
    fn launch_times_walk_synoptic_hours() {
        use chrono::TimeZone;
        let when = Utc.with_ymd_and_hms(2026, 6, 11, 18, 30, 0).unwrap();
        let times = launch_times_before(when);
        assert_eq!(times[0].hour(), 12);
        assert_eq!(times[1].hour(), 0);
        // Early morning before 00z data exists -> walks to previous day.
        let early = Utc.with_ymd_and_hms(2026, 6, 11, 0, 30, 0).unwrap();
        let times = launch_times_before(early);
        assert_eq!(times[0].hour(), 0);
    }
}
