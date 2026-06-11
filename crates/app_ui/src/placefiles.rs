//! GRLevelX-style placefile support: the community-standard overlay format
//! (SpotterNetwork positions, chaser feeds, mesoanalysis contours, local
//! storm reports). This module parses the text format into geolocated draw
//! objects; fetching runs on background threads and drawing goes through the
//! app's view-keyed shape cache, so the UI thread never blocks on a network
//! fetch or re-tessellation.
//!
//! Supported: Title, Refresh, Color, Threshold, Font, Place, Text, Icon
//! (with real IconFile sprite sheets, fetched and sliced), Line, Polygon,
//! and `Object:` blocks (statements inside draw at pixel offsets from the
//! anchor, +x east / +y north, per the GR convention).

/// One parsed placefile.
#[derive(Clone, Debug, Default)]
pub struct Placefile {
    pub title: String,
    pub refresh_minutes: u32,
    pub objects: Vec<PlacefileObject>,
    /// Icon sprite sheets referenced by Icon statements.
    pub icon_sheets: Vec<IconSheetSpec>,
    /// Unrecognized statements (for the honest status line).
    pub skipped: usize,
}

/// `IconFile: index, iconWidth, iconHeight, hotX, hotY, url`
#[derive(Clone, Debug, PartialEq)]
pub struct IconSheetSpec {
    pub index: u32,
    pub icon_w: u32,
    pub icon_h: u32,
    pub hot_x: f32,
    pub hot_y: f32,
    pub url: String,
}

/// When `anchor` is Some, positional fields hold PIXEL OFFSETS from the
/// anchor's screen position (+x east, +y north) instead of lat/lon — the
/// `Object:` block convention used for station plots.
#[derive(Clone, Debug)]
pub enum PlacefileObject {
    Icon {
        lat: f32,
        lon: f32,
        anchor: Option<(f32, f32)>,
        heading_deg: f32,
        file_index: u32,
        icon_index: u32,
        label: Option<String>,
        color: [u8; 3],
        threshold_nm: f32,
    },
    Text {
        lat: f32,
        lon: f32,
        anchor: Option<(f32, f32)>,
        size_px: f32,
        text: String,
        color: [u8; 3],
        threshold_nm: f32,
    },
    Line {
        width: f32,
        points: Vec<(f32, f32)>, // (lat, lon) — or px offsets when anchored
        anchor: Option<(f32, f32)>,
        color: [u8; 3],
        threshold_nm: f32,
    },
    Polygon {
        points: Vec<(f32, f32)>,
        anchor: Option<(f32, f32)>,
        color: [u8; 3],
        threshold_nm: f32,
    },
}

impl PlacefileObject {
    pub fn threshold_nm(&self) -> f32 {
        match self {
            Self::Icon { threshold_nm, .. }
            | Self::Text { threshold_nm, .. }
            | Self::Line { threshold_nm, .. }
            | Self::Polygon { threshold_nm, .. } => *threshold_nm,
        }
    }
}

/// Parse placefile text. Tolerant: unknown statements are ignored, malformed
/// lines are skipped, and a file with no recognized objects still returns
/// (with `objects` empty) so the UI can show an honest status.
pub fn parse_placefile(text: &str, base_url: &str) -> Placefile {
    let mut out = Placefile {
        title: String::new(),
        refresh_minutes: 5,
        objects: Vec::new(),
        icon_sheets: Vec::new(),
        skipped: 0,
    };
    let mut color: [u8; 3] = [255, 255, 255];
    let mut threshold_nm: f32 = 999.0;
    // TimeRange gating (parse-time approximation: the file refreshes on
    // its cadence, so out-of-window items reappear within one refresh of
    // entering their window — matches GR semantics to that bound).
    let mut in_time_window = true;
    let mut hsluv_mode = false;
    let mut fonts: Vec<(u32, f32)> = Vec::new();
    let mut pending_line: Option<(f32, Vec<(f32, f32)>)> = None;
    let mut pending_polygon: Option<Vec<(f32, f32)>> = None;
    let mut object_anchor: Option<(f32, f32)> = None;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with("//") {
            continue;
        }
        let (key, value) = match line.split_once(':') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim()),
            None => (String::new(), line),
        };

        // Coordinate rows belong to an open Line/Polygon. (Inside an Object
        // block these are pixel offsets; validation is relaxed accordingly.)
        if key.is_empty() || key.parse::<f64>().is_ok() {
            if let Some(pair) = parse_pair(line, object_anchor.is_some())
                && let Some(sink) = pending_line
                    .as_mut()
                    .map(|(_, points)| points)
                    .or(pending_polygon.as_mut())
            {
                sink.push(pair);
            }
            continue;
        }

        match key.as_str() {
            "title" => out.title = value.to_owned(),
            "refresh" => {
                if let Ok(minutes) = value.parse::<u32>() {
                    out.refresh_minutes = minutes.max(1);
                }
            }
            "refreshseconds" => {
                // Seconds-granularity refresh (Supercell-Wx parity); our
                // scheduler is minute-based, so round up.
                if let Ok(seconds) = value.parse::<u32>() {
                    out.refresh_minutes = seconds.div_ceil(60).max(1);
                }
            }
            "timerange" => {
                // TimeRange: YYYY-MM-DDThh:mm:ss YYYY-MM-DDThh:mm:ss (UTC)
                let mut parts = value.split_whitespace();
                let parse_t =
                    |t: &str| chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S").ok();
                if let (Some(start), Some(end)) = (
                    parts.next().and_then(parse_t),
                    parts.next().and_then(parse_t),
                ) {
                    let now = chrono::Utc::now().naive_utc();
                    in_time_window = now >= start && now <= end;
                } else {
                    in_time_window = true;
                }
            }
            "hsluv" => {
                hsluv_mode = value.eq_ignore_ascii_case("true");
            }
            "color" => {
                if hsluv_mode {
                    let parts: Vec<f64> = value
                        .split_whitespace()
                        .filter_map(|p| p.parse::<f64>().ok())
                        .collect();
                    if parts.len() >= 3 {
                        color = hsluv_to_rgb(parts[0], parts[1], parts[2]);
                    }
                } else {
                    let parts: Vec<u8> = value
                        .split_whitespace()
                        .filter_map(|p| p.parse::<u8>().ok())
                        .collect();
                    if parts.len() >= 3 {
                        color = [parts[0], parts[1], parts[2]];
                    }
                }
            }
            "threshold" => {
                if let Ok(nm) = value.parse::<f32>() {
                    threshold_nm = nm.max(0.0);
                }
            }
            "font" => {
                let parts: Vec<&str> = value.split(',').collect();
                if parts.len() >= 2
                    && let (Ok(id), Ok(px)) = (
                        parts[0].trim().parse::<u32>(),
                        parts[1].trim().parse::<f32>(),
                    )
                {
                    fonts.retain(|(existing, _)| *existing != id);
                    fonts.push((id, px.clamp(7.0, 32.0)));
                }
            }
            "iconfile" => {
                // IconFile: index, width, height, hotX, hotY, url
                let parts: Vec<&str> = value.splitn(6, ',').collect();
                if parts.len() == 6
                    && let (Ok(index), Ok(w), Ok(h)) = (
                        parts[0].trim().parse::<u32>(),
                        parts[1].trim().parse::<u32>(),
                        parts[2].trim().parse::<u32>(),
                    )
                {
                    let hot_x = parts[3].trim().parse::<f32>().unwrap_or(0.0);
                    let hot_y = parts[4].trim().parse::<f32>().unwrap_or(0.0);
                    let url = resolve_url(base_url, &unquote(parts[5]));
                    if w > 0 && h > 0 && url.starts_with("http") {
                        out.icon_sheets.retain(|sheet| sheet.index != index);
                        out.icon_sheets.push(IconSheetSpec {
                            index,
                            icon_w: w,
                            icon_h: h,
                            hot_x,
                            hot_y,
                            url,
                        });
                    }
                }
            }
            "icon" => {
                // Icon: lat, lon, angle, fileNumber, iconNumber [, hover]
                let parts: Vec<&str> = value.splitn(6, ',').collect();
                if parts.len() >= 5
                    && let Some((lat, lon)) = parse_first_pair(&parts, object_anchor.is_some())
                {
                    let heading = parts[2].trim().parse::<f32>().unwrap_or(0.0);
                    let file_index = parts[3].trim().parse::<u32>().unwrap_or(0);
                    let icon_index = parts[4].trim().parse::<u32>().unwrap_or(1);
                    let label = parts
                        .get(5)
                        .map(|s| unquote(s))
                        .filter(|s| !s.is_empty())
                        .map(|s| s.lines().next().unwrap_or_default().to_owned());
                    push_item(
                        &mut out,
                        in_time_window,
                        PlacefileObject::Icon {
                            lat,
                            lon,
                            anchor: object_anchor,
                            heading_deg: heading,
                            file_index,
                            icon_index,
                            label,
                            color,
                            threshold_nm,
                        },
                    );
                }
            }
            "text" => {
                // Text: lat, lon, fontNumber, "string" [, "hover"]
                let parts: Vec<&str> = value.splitn(4, ',').collect();
                if parts.len() >= 4
                    && let Some((lat, lon)) = parse_first_pair(&parts, object_anchor.is_some())
                {
                    let font_id = parts[2].trim().parse::<u32>().unwrap_or(1);
                    let size = fonts
                        .iter()
                        .find(|(id, _)| *id == font_id)
                        .map(|(_, px)| *px)
                        .unwrap_or(11.0);
                    let text = unquote(parts[3].split(',').next().unwrap_or(parts[3]));
                    if !text.is_empty() {
                        push_item(
                            &mut out,
                            in_time_window,
                            PlacefileObject::Text {
                                lat,
                                lon,
                                anchor: object_anchor,
                                size_px: size,
                                text,
                                color,
                                threshold_nm,
                            },
                        );
                    }
                }
            }
            "place" => {
                let parts: Vec<&str> = value.splitn(3, ',').collect();
                if parts.len() >= 3
                    && let (Ok(lat), Ok(lon)) = (
                        parts[0].trim().parse::<f32>(),
                        parts[1].trim().parse::<f32>(),
                    )
                {
                    push_item(
                        &mut out,
                        in_time_window,
                        PlacefileObject::Text {
                            lat,
                            lon,
                            anchor: None,
                            size_px: 11.0,
                            text: unquote(parts[2]),
                            color,
                            threshold_nm,
                        },
                    );
                }
            }
            "line" => {
                let width = value
                    .split(',')
                    .next()
                    .and_then(|w| w.trim().parse::<f32>().ok())
                    .unwrap_or(1.5)
                    .clamp(0.5, 8.0);
                pending_line = Some((width, Vec::new()));
            }
            "polygon" => pending_polygon = Some(Vec::new()),
            "object" => {
                // Object: lat, lon — subsequent coordinates are pixel offsets.
                let parts: Vec<&str> = value.splitn(2, ',').collect();
                if parts.len() == 2
                    && let (Ok(lat), Ok(lon)) = (
                        parts[0].trim().parse::<f32>(),
                        parts[1].trim().parse::<f32>(),
                    )
                {
                    object_anchor = Some((lat, lon));
                } else {
                    out.skipped += 1;
                }
            }
            "end" => {
                // End: closes the innermost construct: open geometry first,
                // then the Object block.
                if let Some((width, points)) = pending_line.take() {
                    if points.len() >= 2 {
                        push_item(
                            &mut out,
                            in_time_window,
                            PlacefileObject::Line {
                                width,
                                points,
                                anchor: object_anchor,
                                color,
                                threshold_nm,
                            },
                        );
                    }
                } else if let Some(points) = pending_polygon.take() {
                    if points.len() >= 3 {
                        push_item(
                            &mut out,
                            in_time_window,
                            PlacefileObject::Polygon {
                                points,
                                anchor: object_anchor,
                                color,
                                threshold_nm,
                            },
                        );
                    }
                } else {
                    object_anchor = None;
                }
            }
            _ => out.skipped += 1,
        }
    }
    if let Some((width, points)) = pending_line.take()
        && points.len() >= 2
    {
        push_item(
            &mut out,
            in_time_window,
            PlacefileObject::Line {
                width,
                points,
                anchor: object_anchor,
                color,
                threshold_nm,
            },
        );
    }
    if let Some(points) = pending_polygon.take()
        && points.len() >= 3
    {
        push_item(
            &mut out,
            in_time_window,
            PlacefileObject::Polygon {
                points,
                anchor: object_anchor,
                color,
                threshold_nm,
            },
        );
    }
    out
}

/// Parse the first two comma fields as a coordinate pair. In geo mode the
/// pair is validated as lat/lon; in offset (Object) mode any finite numbers
/// within ±4096 px pass.
fn parse_first_pair(parts: &[&str], offsets: bool) -> Option<(f32, f32)> {
    let a = parts.first()?.trim().parse::<f32>().ok()?;
    let b = parts.get(1)?.trim().parse::<f32>().ok()?;
    pair_valid(a, b, offsets).then_some((a, b))
}

fn parse_pair(line: &str, offsets: bool) -> Option<(f32, f32)> {
    let mut parts = line.split(',');
    let a = parts.next()?.trim().parse::<f32>().ok()?;
    let b = parts.next()?.trim().parse::<f32>().ok()?;
    pair_valid(a, b, offsets).then_some((a, b))
}

fn pair_valid(a: f32, b: f32, offsets: bool) -> bool {
    if offsets {
        a.is_finite() && b.is_finite() && a.abs() <= 4096.0 && b.abs() <= 4096.0
    } else {
        (-90.0..=90.0).contains(&a) && (-180.0..=180.0).contains(&b)
    }
}

fn unquote(value: &str) -> String {
    value.trim().trim_matches('"').trim().to_owned()
}

/// Push an item unless the active TimeRange excludes the current moment.
fn push_item(out: &mut Placefile, in_time_window: bool, object: PlacefileObject) {
    if in_time_window {
        out.objects.push(object);
    } else {
        out.skipped += 1;
    }
}

/// Resolve a possibly-relative IconFile URL against the placefile URL
/// (Supercell-Wx parity: "icons/x.png" loads from the placefile's host).
fn resolve_url(base_url: &str, raw: &str) -> String {
    if raw.starts_with("http://") || raw.starts_with("https://") || base_url.is_empty() {
        return raw.to_owned();
    }
    if let Some(scheme_end) = base_url.find("://") {
        let host_start = scheme_end + 3;
        if raw.starts_with('/') {
            // Host-absolute path.
            let host_end = base_url[host_start..]
                .find('/')
                .map(|i| host_start + i)
                .unwrap_or(base_url.len());
            return format!("{}{}", &base_url[..host_end], raw);
        }
        // Relative to the placefile's directory.
        let dir_end = base_url.rfind('/').filter(|&i| i > host_start);
        if let Some(end) = dir_end {
            return format!("{}/{}", &base_url[..end], raw);
        }
        return format!("{base_url}/{raw}");
    }
    raw.to_owned()
}

// ---- HSLuv -> sRGB (reference implementation; www.hsluv.org) ----
// H in [0,360], S and L in [0,100]. Needed because GR placefiles can
// switch Color: into HSLuv space via the "HSLuv: true" directive.

const HSLUV_M: [[f64; 3]; 3] = [
    [3.240969941904521, -1.537383177570093, -0.498610760293],
    [-0.96924363628087, 1.87596750150772, 0.041555057407175],
    [0.055630079696993, -0.20397695888897, 1.056971514242878],
];
const HSLUV_REF_U: f64 = 0.19783000664283;
const HSLUV_REF_V: f64 = 0.46831999493879;
const HSLUV_KAPPA: f64 = 903.2962962;
const HSLUV_EPS: f64 = 0.0088564516;

fn hsluv_max_chroma(l: f64, h_deg: f64) -> f64 {
    let hrad = h_deg.to_radians();
    let sub1 = (l + 16.0).powi(3) / 1_560_896.0;
    let sub2 = if sub1 > HSLUV_EPS {
        sub1
    } else {
        l / HSLUV_KAPPA
    };
    let mut min_len = f64::MAX;
    for m in &HSLUV_M {
        for t in 0..2 {
            let t = t as f64;
            let top1 = (284_517.0 * m[0] - 94_839.0 * m[2]) * sub2;
            let top2 = (838_422.0 * m[2] + 769_860.0 * m[1] + 731_718.0 * m[0]) * l * sub2
                - 769_860.0 * t * l;
            let bottom = (632_260.0 * m[2] - 126_452.0 * m[1]) * sub2 + 126_452.0 * t;
            if bottom.abs() < 1e-12 {
                continue;
            }
            let slope = top1 / bottom;
            let intercept = top2 / bottom;
            let denom = hrad.sin() - slope * hrad.cos();
            if denom.abs() < 1e-12 {
                continue;
            }
            let len = intercept / denom;
            if len >= 0.0 {
                min_len = min_len.min(len);
            }
        }
    }
    min_len
}

fn hsluv_to_rgb(h: f64, s: f64, l: f64) -> [u8; 3] {
    let l = l.clamp(0.0, 100.0);
    let s = s.clamp(0.0, 100.0);
    if l > 99.999 {
        return [255, 255, 255];
    }
    if l < 0.001 {
        return [0, 0, 0];
    }
    // LCh
    let c = hsluv_max_chroma(l, h) / 100.0 * s;
    let hrad = h.to_radians();
    let (u, v) = (c * hrad.cos(), c * hrad.sin());
    // Luv -> XYZ
    let var_y = if l > 8.0 {
        ((l + 16.0) / 116.0).powi(3)
    } else {
        l / HSLUV_KAPPA
    };
    let var_u = u / (13.0 * l) + HSLUV_REF_U;
    let var_v = v / (13.0 * l) + HSLUV_REF_V;
    let y = var_y;
    let x = -(9.0 * y * var_u) / ((var_u - 4.0) * var_v - var_u * var_v);
    let z = (9.0 * y - 15.0 * var_v * y - var_v * x) / (3.0 * var_v);
    // XYZ -> sRGB
    let gamma = |c: f64| -> f64 {
        if c <= 0.0031308 {
            12.92 * c
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    };
    let channel = |m: &[f64; 3]| -> u8 {
        (gamma(m[0] * x + m[1] * y + m[2] * z).clamp(0.0, 1.0) * 255.0).round() as u8
    };
    [
        channel(&HSLUV_M[0]),
        channel(&HSLUV_M[1]),
        channel(&HSLUV_M[2]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
; spotter feed sample
Title: Test Spotters
Refresh: 2
Color: 255 0 0
Threshold: 999
Font: 1, 12, 1, "Arial"
IconFile: 1, 16, 16, 8, 8, "http://example/icons.png"
Icon: 39.05, -94.59, 45, 1, 3, "Spotter One\nReporting"
Text: 38.90, -94.20, 1, "KC METAR", "hover"
Place: 38.50, -94.00, Old Style Label
Color: 0 128 255
Line: 2, 0
 39.0, -95.0
 39.2, -94.8
 39.4, -94.6
End:
Polygon:
 38.0, -95.0
 38.2, -94.8
 38.0, -94.6
End:
Object: 39.0, -94.0
 Icon: 0, 0, 0, 1, 1
 Text: 10, -12, 1, "T"
End:
"#;

    #[test]
    fn parses_the_core_statements() {
        let pf = parse_placefile(SAMPLE, "");
        assert_eq!(pf.title, "Test Spotters");
        assert_eq!(pf.refresh_minutes, 2);
        assert_eq!(pf.icon_sheets.len(), 1);
        assert_eq!(pf.icon_sheets[0].icon_w, 16);
        assert_eq!(pf.icon_sheets[0].url, "http://example/icons.png");
        assert_eq!(pf.objects.len(), 7, "{:#?}", pf.objects);
        match &pf.objects[0] {
            PlacefileObject::Icon {
                lat,
                lon,
                anchor,
                heading_deg,
                file_index,
                icon_index,
                label,
                color,
                ..
            } => {
                assert!((lat - 39.05).abs() < 1e-4);
                assert!((lon + 94.59).abs() < 1e-4);
                assert!(anchor.is_none());
                assert_eq!(*heading_deg, 45.0);
                assert_eq!(*file_index, 1);
                assert_eq!(*icon_index, 3);
                assert_eq!(label.as_deref(), Some("Spotter One\\nReporting"));
                assert_eq!(*color, [255, 0, 0]);
            }
            other => panic!("expected icon, got {other:?}"),
        }
        match &pf.objects[3] {
            PlacefileObject::Line { width, points, .. } => {
                assert_eq!(*width, 2.0);
                assert_eq!(points.len(), 3);
            }
            other => panic!("expected line, got {other:?}"),
        }
        // Object-block members carry the anchor with pixel offsets.
        match &pf.objects[5] {
            PlacefileObject::Icon {
                lat, lon, anchor, ..
            } => {
                assert_eq!((*lat, *lon), (0.0, 0.0));
                assert_eq!(*anchor, Some((39.0, -94.0)));
            }
            other => panic!("expected anchored icon, got {other:?}"),
        }
        match &pf.objects[6] {
            PlacefileObject::Text {
                lat, lon, anchor, ..
            } => {
                assert_eq!((*lat, *lon), (10.0, -12.0));
                assert_eq!(*anchor, Some((39.0, -94.0)));
            }
            other => panic!("expected anchored text, got {other:?}"),
        }
    }

    #[test]
    fn object_anchor_resets_after_end() {
        let pf = parse_placefile(
            "Object: 39.0, -94.0\n Icon: 0, 0, 0, 1, 1\nEnd:\nIcon: 38.0, -95.0, 0, 1, 1\n",
            "",
        );
        assert_eq!(pf.objects.len(), 2);
        match &pf.objects[1] {
            PlacefileObject::Icon { anchor, .. } => assert!(anchor.is_none()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn malformed_lines_are_skipped_not_fatal() {
        let pf = parse_placefile(
            "Title: x\nIcon: not, numbers\nText: 1,2\nLine: zz\nEnd:\n",
            "",
        );
        assert_eq!(pf.title, "x");
        assert!(pf.objects.is_empty());
    }
}
