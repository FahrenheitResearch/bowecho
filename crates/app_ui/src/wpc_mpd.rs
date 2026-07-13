//! Live Weather Prediction Center Mesoscale Precipitation Discussions.
//!
//! The NWS Telecommunications Gateway exposes WPC's current `FFGMPD` product
//! as a raw operational text feed. This avoids both HTML scraping and the
//! mixed-purpose `FFG` API collection (which is sometimes empty).

use crate::*;

const WPC_MPD_RAW_FEED_URL: &str = "https://tgftp.nws.noaa.gov/data/raw/aw/awus01.kwnh.ffg.mpd.txt";

pub(crate) fn load_live(query_time_utc: DateTime<Utc>) -> Result<SpcMdLoad, String> {
    let text = data_source::fetch_text(WPC_MPD_RAW_FEED_URL)
        .map_err(|err| format!("WPC MPD raw feed fetch failed: {err}"))?;
    let issuance_time = parse_wmo_issuance_time(&text, query_time_utc).unwrap_or(query_time_utc);
    let records = parse_product_text(&text, issuance_time, query_time_utc);
    let parsed_items = usize::from(!records.is_empty());
    let error_count = usize::from(records.is_empty());

    Ok(SpcMdLoad {
        scanned_items: 1,
        parsed_items,
        error_count,
        records,
    })
}

fn parse_wmo_issuance_time(text: &str, reference: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let token = text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        if parts.next() == Some("AWUS01") && parts.next() == Some("KWNH") {
            parts.next()
        } else {
            None
        }
    })?;
    day_time_near(token, reference, -20, 20)
}

fn parse_product_text(
    text: &str,
    issuance_time: DateTime<Utc>,
    query_time_utc: DateTime<Utc>,
) -> Vec<HazardRecord> {
    let upper = text.to_ascii_uppercase();
    if !upper.contains("AWUS01 KWNH")
        || !upper.contains("FFGMPD")
        || !upper.contains("MESOSCALE PRECIPITATION DISCUSSION")
    {
        return Vec::new();
    }
    let Some(number) = first_number_after(&upper, "MESOSCALE PRECIPITATION DISCUSSION") else {
        return Vec::new();
    };
    let Some(valid_line) = text
        .lines()
        .map(str::trim)
        .find(|line| line.to_ascii_uppercase().starts_with("VALID "))
    else {
        return Vec::new();
    };
    let Some((valid_start, valid_end)) = parse_valid_window(valid_line, issuance_time) else {
        return Vec::new();
    };
    let rings = parse_lat_lon_rings(text);
    let area = prefixed_value(text, "Areas affected...");
    let concerning = prefixed_value(text, "Concerning...");
    let lifecycle_status = if query_time_utc < valid_start {
        "Pending"
    } else if query_time_utc >= valid_end {
        "Expired"
    } else {
        "Active"
    };
    let mut details = vec![
        valid_line.split_whitespace().collect::<Vec<_>>().join(" "),
        format!("Issued {}", format_utc_seconds(issuance_time)),
    ];
    if let Some(summary) = prefixed_value(text, "SUMMARY...") {
        details.push(summary);
    }
    let number_value = number.parse::<u16>().unwrap_or_default();
    let source_url = format!(
        "https://www.wpc.ncep.noaa.gov/metwatch/metwatch_mpd_multi.php?md={number_value:04}&yr={}",
        valid_start.year()
    );
    let ring_count = rings.len();

    rings
        .into_iter()
        .enumerate()
        .filter(|(_, points)| points.len() >= 3)
        .map(|(index, points)| HazardRecord {
            event_id: if ring_count > 1 {
                format!("wpc-mpd-{number}-ring-{}", index + 1)
            } else {
                format!("wpc-mpd-{number}")
            },
            label: format!("MPD {number}"),
            // Share the existing MD filter/style while the label and office
            // distinguish WPC precipitation discussions from SPC MDs.
            event_family: "mesoscale discussion".to_owned(),
            action: "WPC".to_owned(),
            lifecycle_status: Some(lifecycle_status.to_owned()),
            office: "WPC".to_owned(),
            headline: concerning.clone(),
            source_url: Some(source_url.clone()),
            area: area.clone(),
            motion: None,
            details: details.clone(),
            valid_start: Some(format_utc_seconds(valid_start)),
            valid_end: Some(format_utc_seconds(valid_end)),
            severity: None,
            certainty: None,
            urgency: None,
            tornado: None,
            hail_inches: None,
            wind_mph: None,
            damage_threat: None,
            bbox: bbox(&points),
            points,
        })
        .collect()
}

fn first_number_after(text: &str, marker: &str) -> Option<String> {
    let offset = text.find(marker)? + marker.len();
    text[offset..]
        .split(|character: char| !character.is_ascii_digit())
        .find(|token| !token.is_empty())
        .map(|token| {
            let trimmed = token.trim_start_matches('0');
            if trimmed.is_empty() { "0" } else { trimmed }.to_owned()
        })
}

fn prefixed_value(text: &str, prefix: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let line = line.trim();
        (line.len() >= prefix.len() && line[..prefix.len()].eq_ignore_ascii_case(prefix))
            .then(|| line[prefix.len()..].trim().to_owned())
            .filter(|value| !value.is_empty())
    })
}

fn parse_valid_window(
    line: &str,
    reference: DateTime<Utc>,
) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let tokens = line
        .split(|character: char| character.is_whitespace() || character == '-')
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    let start_token = tokens.iter().find(|token| is_day_time_token(token))?;
    let end_token = tokens
        .iter()
        .skip_while(|token| *token != start_token)
        .skip(1)
        .find(|token| is_day_time_token(token))?;
    let start = day_time_near(start_token, reference, -20, 20)?;
    let end = day_time_near(end_token, start, 0, 3)?;
    Some((start, end))
}

fn is_day_time_token(token: &str) -> bool {
    let value = token.trim_end_matches(['Z', 'z']);
    value.len() == 6 && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn day_time_near(
    token: &str,
    reference: DateTime<Utc>,
    minimum_day_offset: i64,
    maximum_day_offset: i64,
) -> Option<DateTime<Utc>> {
    let value = token.trim_end_matches(['Z', 'z']);
    let day = value[..2].parse::<u32>().ok()?;
    let hour = value[2..4].parse::<u32>().ok()?;
    let minute = value[4..6].parse::<u32>().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    (minimum_day_offset..=maximum_day_offset)
        .filter_map(|offset| {
            let date = reference.date_naive() + chrono::Duration::days(offset);
            if date.day() != day {
                return None;
            }
            date.and_hms_opt(hour, minute, 0)
                .map(|datetime| Utc.from_utc_datetime(&datetime))
        })
        .min_by_key(|candidate| (*candidate - reference).num_seconds().abs())
}

fn parse_lat_lon_rings(text: &str) -> Vec<Vec<HazardPoint>> {
    let lines = text.lines().collect::<Vec<_>>();
    let Some(start_index) = lines
        .iter()
        .position(|line| line.trim_start().starts_with("LAT...LON"))
    else {
        return Vec::new();
    };
    let mut rings = Vec::new();
    let mut points = Vec::new();
    for line in &lines[start_index..] {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "$$" {
            if points.len() >= 3 {
                rings.push(std::mem::take(&mut points));
            }
            continue;
        }
        if trimmed.contains("...") && !trimmed.starts_with("LAT...LON") {
            break;
        }
        let body = trimmed.strip_prefix("LAT...LON").unwrap_or(trimmed);
        points.extend(
            body.split_whitespace()
                .filter_map(parse_compact_lat_lon_token),
        );
    }
    if points.len() >= 3 {
        rings.push(points);
    }
    rings
}

fn parse_compact_lat_lon_token(value: &str) -> Option<HazardPoint> {
    if value.len() < 8 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let lat = value[..4].parse::<f32>().ok()? / 100.0;
    let mut longitude = value[4..].parse::<f32>().ok()? / 100.0;
    if longitude < 60.0 {
        longitude += 100.0;
    }
    Some(HazardPoint {
        lon: -longitude,
        lat,
    })
}

fn bbox(points: &[HazardPoint]) -> [f32; 4] {
    points.iter().fold(
        [
            f32::INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NEG_INFINITY,
        ],
        |[min_lon, min_lat, max_lon, max_lat], point| {
            [
                min_lon.min(point.lon),
                min_lat.min(point.lat),
                max_lon.max(point.lon),
                max_lat.max(point.lat),
            ]
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wmo_issuance_time_from_raw_feed_heading() {
        let query = Utc.with_ymd_and_hms(2026, 7, 13, 4, 0, 0).unwrap();
        assert_eq!(
            parse_wmo_issuance_time("000\nAWUS01 KWNH 130239\nFFGMPD", query),
            Some(Utc.with_ymd_and_hms(2026, 7, 13, 2, 39, 0).unwrap())
        );
    }

    #[test]
    fn parses_machine_feed_mpd_geometry_attribution_and_validity() {
        let text = r#"
000
AWUS01 KWNH 130239
FFGMPD
Mesoscale Precipitation Discussion 0693
NWS Weather Prediction Center College Park MD
Areas affected...Central NC
Concerning...Heavy rainfall...Flash flooding likely
Valid 130238Z - 130700Z
SUMMARY...Training storms may produce flash flooding.
LAT...LON   36077945 35967851 35767740 35347651
"#;
        let issued = Utc.with_ymd_and_hms(2026, 7, 13, 2, 39, 0).unwrap();
        let query = Utc.with_ymd_and_hms(2026, 7, 13, 4, 0, 0).unwrap();
        let records = parse_product_text(text, issued, query);
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.event_id, "wpc-mpd-693");
        assert_eq!(record.label, "MPD 693");
        assert_eq!(record.office, "WPC");
        assert_eq!(record.lifecycle_status.as_deref(), Some("Active"));
        assert_eq!(record.valid_start.as_deref(), Some("2026-07-13T02:38:00Z"));
        assert_eq!(record.valid_end.as_deref(), Some("2026-07-13T07:00:00Z"));
        assert_eq!(
            record.points[0],
            HazardPoint {
                lon: -79.45,
                lat: 36.07
            }
        );
    }

    #[test]
    fn preserves_multiple_polygon_rings_from_one_product() {
        let text = r#"
AWUS01 KWNH 130239
FFGMPD
Mesoscale Precipitation Discussion 0693
Valid 130238Z - 130700Z
LAT...LON   36531314 35881250 34531180

            36077945 35967851 35767740
"#;
        let issued = Utc.with_ymd_and_hms(2026, 7, 13, 2, 39, 0).unwrap();
        let records = parse_product_text(text, issued, issued);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].event_id, "wpc-mpd-693-ring-1");
        assert_eq!(records[1].event_id, "wpc-mpd-693-ring-2");
    }

    #[test]
    fn valid_window_handles_month_rollover() {
        let query = Utc.with_ymd_and_hms(2026, 7, 31, 23, 50, 0).unwrap();
        let (start, end) = parse_valid_window("Valid 312330Z - 010330Z", query).unwrap();
        assert_eq!(start, Utc.with_ymd_and_hms(2026, 7, 31, 23, 30, 0).unwrap());
        assert_eq!(end, Utc.with_ymd_and_hms(2026, 8, 1, 3, 30, 0).unwrap());
    }

    #[test]
    fn live_machine_feed_loads_when_enabled() {
        if std::env::var_os("BOWECHO_TEST_LIVE_WPC_MPD").is_none() {
            return;
        }
        let load = load_live(Utc::now()).expect("load official WPC FFGMPD machine feed");
        assert!(load.parsed_items > 0, "live WPC feed yielded no parsed MPD");
        assert!(
            load.records.iter().all(|record| record.office == "WPC"
                && record.label.starts_with("MPD ")
                && record.valid_start.is_some()
                && record.valid_end.is_some()),
            "live WPC records must retain identity, attribution, and validity"
        );
    }
}
