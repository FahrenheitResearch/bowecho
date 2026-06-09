//! GRLevelX-style placefile support: the community-standard overlay format
//! (SpotterNetwork positions, chaser feeds, mesoanalysis contours, local
//! storm reports). This module parses the text format into geolocated draw
//! objects; fetching runs on background threads and drawing goes through the
//! app's view-keyed shape cache, so the UI thread never blocks on a network
//! fetch or re-tessellation.
//!
//! v1 scope: Title, Refresh, Color, Threshold, Font, Place, Text, Icon
//! (rendered as a heading-ticked dot — remote icon sheets are not fetched),
//! Line and Polygon. `Object:` blocks (pixel-relative drawing) are skipped
//! gracefully.

/// One parsed placefile.
#[derive(Clone, Debug, Default)]
pub struct Placefile {
    pub title: String,
    pub refresh_minutes: u32,
    pub objects: Vec<PlacefileObject>,
    /// Statements we recognized but skipped (e.g. Object blocks).
    pub skipped: usize,
}

#[derive(Clone, Debug)]
pub enum PlacefileObject {
    Icon {
        lat: f32,
        lon: f32,
        heading_deg: f32,
        label: Option<String>,
        color: [u8; 3],
        threshold_nm: f32,
    },
    Text {
        lat: f32,
        lon: f32,
        size_px: f32,
        text: String,
        color: [u8; 3],
        threshold_nm: f32,
    },
    Line {
        width: f32,
        points: Vec<(f32, f32)>, // (lat, lon)
        color: [u8; 3],
        threshold_nm: f32,
    },
    Polygon {
        points: Vec<(f32, f32)>,
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
pub fn parse_placefile(text: &str) -> Placefile {
    let mut out = Placefile {
        title: String::new(),
        refresh_minutes: 5,
        objects: Vec::new(),
        skipped: 0,
    };
    let mut color: [u8; 3] = [255, 255, 255];
    let mut threshold_nm: f32 = 999.0;
    let mut fonts: Vec<(u32, f32)> = Vec::new();
    let mut pending_line: Option<(f32, Vec<(f32, f32)>)> = None;
    let mut pending_polygon: Option<Vec<(f32, f32)>> = None;
    let mut skipping_object = false;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with("//") {
            continue;
        }
        let (key, value) = match line.split_once(':') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim()),
            None => (String::new(), line),
        };

        if skipping_object {
            if key == "end" {
                skipping_object = false;
            }
            continue;
        }

        // Coordinate rows belong to an open Line/Polygon.
        if key.is_empty() || key.parse::<f64>().is_ok() {
            if let Some((lat, lon)) = parse_lat_lon_pair(line)
                && let Some(sink) = pending_line
                    .as_mut()
                    .map(|(_, points)| points)
                    .or(pending_polygon.as_mut())
            {
                sink.push((lat, lon));
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
            "color" => {
                let parts: Vec<u8> = value
                    .split_whitespace()
                    .filter_map(|p| p.parse::<u8>().ok())
                    .collect();
                if parts.len() >= 3 {
                    color = [parts[0], parts[1], parts[2]];
                }
            }
            "threshold" => {
                if let Ok(nm) = value.parse::<f32>() {
                    threshold_nm = nm.max(0.0);
                }
            }
            "font" => {
                // Font: id, pixels, flags, "face"
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
            "iconfile" => { /* icon sheets not fetched; Icon renders as a dot */ }
            "icon" => {
                // Icon: lat, lon, angle, fileNumber, iconNumber [, hover]
                let parts: Vec<&str> = value.splitn(6, ',').collect();
                if parts.len() >= 5
                    && let (Ok(lat), Ok(lon)) = (
                        parts[0].trim().parse::<f32>(),
                        parts[1].trim().parse::<f32>(),
                    )
                {
                    let heading = parts[2].trim().parse::<f32>().unwrap_or(0.0);
                    let label = parts
                        .get(5)
                        .map(|s| unquote(s))
                        .filter(|s| !s.is_empty())
                        // Hover text often packs multiple lines; keep the first.
                        .map(|s| s.lines().next().unwrap_or_default().to_owned());
                    out.objects.push(PlacefileObject::Icon {
                        lat,
                        lon,
                        heading_deg: heading,
                        label,
                        color,
                        threshold_nm,
                    });
                }
            }
            "text" => {
                // Text: lat, lon, fontNumber, "string" [, "hover"]
                let parts: Vec<&str> = value.splitn(4, ',').collect();
                if parts.len() >= 4
                    && let (Ok(lat), Ok(lon)) = (
                        parts[0].trim().parse::<f32>(),
                        parts[1].trim().parse::<f32>(),
                    )
                {
                    let font_id = parts[2].trim().parse::<u32>().unwrap_or(1);
                    let size = fonts
                        .iter()
                        .find(|(id, _)| *id == font_id)
                        .map(|(_, px)| *px)
                        .unwrap_or(11.0);
                    let text = unquote(parts[3].split(',').next().unwrap_or(parts[3]));
                    if !text.is_empty() {
                        out.objects.push(PlacefileObject::Text {
                            lat,
                            lon,
                            size_px: size,
                            text,
                            color,
                            threshold_nm,
                        });
                    }
                }
            }
            "place" => {
                // Place: lat, lon, string (legacy)
                let parts: Vec<&str> = value.splitn(3, ',').collect();
                if parts.len() >= 3
                    && let (Ok(lat), Ok(lon)) = (
                        parts[0].trim().parse::<f32>(),
                        parts[1].trim().parse::<f32>(),
                    )
                {
                    out.objects.push(PlacefileObject::Text {
                        lat,
                        lon,
                        size_px: 11.0,
                        text: unquote(parts[2]),
                        color,
                        threshold_nm,
                    });
                }
            }
            "line" => {
                // Line: width, flags [, hover]  ... coords ... End:
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
                skipping_object = true;
                out.skipped += 1;
            }
            "end" => {
                if let Some((width, points)) = pending_line.take() {
                    if points.len() >= 2 {
                        out.objects.push(PlacefileObject::Line {
                            width,
                            points,
                            color,
                            threshold_nm,
                        });
                    }
                } else if let Some(points) = pending_polygon.take()
                    && points.len() >= 3
                {
                    out.objects.push(PlacefileObject::Polygon {
                        points,
                        color,
                        threshold_nm,
                    });
                }
            }
            _ => {}
        }
    }
    // Unterminated trailing geometry still draws.
    if let Some((width, points)) = pending_line.take()
        && points.len() >= 2
    {
        out.objects.push(PlacefileObject::Line {
            width,
            points,
            color,
            threshold_nm,
        });
    }
    if let Some(points) = pending_polygon.take()
        && points.len() >= 3
    {
        out.objects.push(PlacefileObject::Polygon {
            points,
            color,
            threshold_nm,
        });
    }
    out
}

fn parse_lat_lon_pair(line: &str) -> Option<(f32, f32)> {
    let mut parts = line.split(',');
    let lat = parts.next()?.trim().parse::<f32>().ok()?;
    let lon = parts.next()?.trim().parse::<f32>().ok()?;
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

fn unquote(value: &str) -> String {
    value.trim().trim_matches('"').trim().to_owned()
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
End:
"#;

    #[test]
    fn parses_the_core_statements() {
        let pf = parse_placefile(SAMPLE);
        assert_eq!(pf.title, "Test Spotters");
        assert_eq!(pf.refresh_minutes, 2);
        assert_eq!(pf.skipped, 1, "Object block should be skipped");
        assert_eq!(pf.objects.len(), 5);
        match &pf.objects[0] {
            PlacefileObject::Icon {
                lat,
                lon,
                heading_deg,
                label,
                color,
                ..
            } => {
                assert!((lat - 39.05).abs() < 1e-4);
                assert!((lon + 94.59).abs() < 1e-4);
                assert_eq!(*heading_deg, 45.0);
                assert_eq!(label.as_deref(), Some("Spotter One\\nReporting"));
                assert_eq!(*color, [255, 0, 0]);
            }
            other => panic!("expected icon, got {other:?}"),
        }
        match &pf.objects[1] {
            PlacefileObject::Text { text, size_px, .. } => {
                assert_eq!(text, "KC METAR");
                assert_eq!(*size_px, 12.0);
            }
            other => panic!("expected text, got {other:?}"),
        }
        match &pf.objects[3] {
            PlacefileObject::Line {
                width,
                points,
                color,
                ..
            } => {
                assert_eq!(*width, 2.0);
                assert_eq!(points.len(), 3);
                assert_eq!(*color, [0, 128, 255]);
            }
            other => panic!("expected line, got {other:?}"),
        }
        match &pf.objects[4] {
            PlacefileObject::Polygon { points, .. } => assert_eq!(points.len(), 3),
            other => panic!("expected polygon, got {other:?}"),
        }
    }

    #[test]
    fn malformed_lines_are_skipped_not_fatal() {
        let pf = parse_placefile("Title: x\nIcon: not, numbers\nText: 1,2\nLine: zz\nEnd:\n");
        assert_eq!(pf.title, "x");
        assert!(pf.objects.is_empty());
    }
}
